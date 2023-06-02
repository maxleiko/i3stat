use std::error::Error;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use hex_color::HexColor;
use iwlib::WirelessInfo;
use serde::{de, Deserialize, Serialize};

use crate::context::{BarItem, Context};
use crate::dbus::dbus_connection;
use crate::dbus::network_manager::NetworkManagerProxy;
use crate::format::fraction;
use crate::i3::{I3Item, I3Markup};
use crate::net::{Interface, InterfaceKind};
use crate::theme::Theme;

impl Interface {
    fn format_wireless(&self, i: WirelessInfo, theme: &Theme) -> (String, Option<HexColor>) {
        let fg = match i.wi_quality {
            100..=u8::MAX => theme.green,
            80..=99 => theme.green,
            60..=79 => theme.yellow,
            40..=59 => theme.orange,
            _ => theme.red,
        };

        (
            format!("({}) {}% at {}", self.addr, i.wi_quality, i.wi_essid),
            Some(fg),
        )
    }

    fn format_normal(&self, theme: &Theme) -> (String, Option<HexColor>) {
        (format!("({})", self.addr), Some(theme.green))
    }

    fn format(&mut self, theme: &Theme) -> (String, String) {
        let (addr, fg) = match self.get_wireless_info() {
            Some(info) => self.format_wireless(info, theme),
            None => self.format_normal(theme),
        };

        let fg = fg
            .map(|c| format!(r#" foreground="{}""#, c))
            .unwrap_or("".into());
        (
            format!(r#"<span{}>{}{}</span>"#, fg, self.name, addr),
            format!(r#"<span{}>{}</span>"#, fg, self.name),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InterfaceFilter {
    name: String,
    kind: Option<InterfaceKind>,
}

impl InterfaceFilter {
    fn new(name: impl AsRef<str>, kind: Option<InterfaceKind>) -> InterfaceFilter {
        InterfaceFilter {
            name: name.as_ref().to_owned(),
            kind,
        }
    }

    fn matches(&self, interface: &Interface) -> bool {
        let name_match = if self.name.is_empty() {
            true
        } else {
            self.name == interface.name
        };

        match self.kind {
            None => name_match,
            Some(k) => name_match && k == interface.kind,
        }
    }
}

impl ToString for InterfaceFilter {
    fn to_string(&self) -> String {
        match self.kind {
            Some(kind) => format!("{}:{}", self.name, kind.to_string()),
            None => self.name.clone(),
        }
    }
}

impl FromStr for InterfaceFilter {
    type Err = Box<dyn Error>;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let d = ':';
        if !s.contains(d) {
            return Ok(InterfaceFilter::new(s, None));
        }

        // SAFETY: we just checked for the delimiter above
        let (name, kind) = s.split_once(d).unwrap();
        match kind.parse() {
            Ok(kind) => Ok(InterfaceFilter::new(name, Some(kind))),
            Err(e) => Err(e),
        }
    }
}

impl Serialize for InterfaceFilter {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InterfaceFilter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.parse::<InterfaceFilter>() {
            Ok(value) => Ok(value),
            Err(e) => Err(de::Error::custom(e)),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Nic {
    #[serde(default, with = "crate::human_time::option")]
    interval: Option<Duration>,
    /// This type is in the format of `interface[:type]`, where `interface` is the interface name, and
    /// `type` is an optional part which is either `ipv4` or `ipv6`.
    ///
    /// If `interface` is an empty string, then all interfaces are matched, for example:
    /// - `vpn0:ipv4` will match ip4 addresses for the `vpn` interface
    /// - `:ipv6`     will match all interfaces which have an ip6 address
    #[serde(default)]
    filter: Vec<InterfaceFilter>,
}

#[async_trait(?Send)]
impl BarItem for Nic {
    async fn start(self: Box<Self>, mut ctx: Context) -> Result<(), Box<dyn Error>> {
        let connection = dbus_connection(crate::dbus::BusType::System).await?;
        let nm = NetworkManagerProxy::new(&connection).await?;
        let mut nm_state_change = nm.receive_state_changed().await?;

        let mut idx = 0;
        loop {
            let mut interfaces = Interface::get_interfaces()?
                .into_iter()
                .filter(|i| {
                    if self.filter.is_empty() {
                        true
                    } else {
                        self.filter.iter().any(|f| f.matches(i))
                    }
                })
                .collect::<Vec<_>>();

            // no networks active
            if interfaces.is_empty() {
                ctx.update_item(I3Item::new("disconnected").color(ctx.theme().red))
                    .await?;

                idx = 0;
                tokio::select! {
                    Some(_) = ctx.wait_for_event(self.interval) => continue,
                    Some(_) = nm_state_change.next() => continue,
                }
            }

            let len = interfaces.len();
            idx = idx % len;

            let theme = ctx.theme();
            let (full, short) = interfaces[idx].format(&theme);
            let full = format!(r#"{}{}"#, full, fraction(&theme, idx + 1, len));

            let item = I3Item::new(full).short_text(short).markup(I3Markup::Pango);
            ctx.update_item(item).await?;

            // cycle through networks on click
            let wait_for_click = async {
                match self.interval {
                    Some(duration) => {
                        ctx.delay_with_event_handler(duration, |event| {
                            Context::paginate(&event, len, &mut idx);
                            async {}
                        })
                        .await
                    }
                    None => {
                        if let Some(event) = ctx.wait_for_event(self.interval).await {
                            Context::paginate(&event, len, &mut idx);
                        }
                    }
                }
            };

            tokio::select! {
                () = wait_for_click => continue,
                Some(_) = nm_state_change.next() => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn interface_filter_to_string() {
        use InterfaceFilter as F;

        assert_eq!(F::new("foo", None).to_string(), "foo");
        assert_eq!(F::new("bar", Some(InterfaceKind::V4)).to_string(), "bar:v4");
        assert_eq!(F::new("baz", Some(InterfaceKind::V6)).to_string(), "baz:v6");
        assert_eq!(F::new("", None).to_string(), "");
        assert_eq!(F::new("", Some(InterfaceKind::V4)).to_string(), ":v4");
        assert_eq!(F::new("", Some(InterfaceKind::V6)).to_string(), ":v6");
    }

    #[test]
    fn interface_filter_from_str() {
        use InterfaceFilter as F;

        let p = |s: &str| s.parse::<F>().unwrap();
        assert_eq!(p("foo"), F::new("foo", None));
        assert_eq!(p("bar:v4"), F::new("bar", Some(InterfaceKind::V4)));
        assert_eq!(p("baz:v6"), F::new("baz", Some(InterfaceKind::V6)));
        assert_eq!(p(""), F::new("", None));
        assert_eq!(p(":v4"), F::new("", Some(InterfaceKind::V4)));
        assert_eq!(p(":v6"), F::new("", Some(InterfaceKind::V6)));
    }

    #[test]
    fn interface_filter_ser() {
        let to_s = |i| serde_json::to_value(&i).unwrap();

        assert_eq!(to_s(InterfaceFilter::new("foo", None)), "foo");
        assert_eq!(
            to_s(InterfaceFilter::new("bar", Some(InterfaceKind::V4))),
            "bar:v4"
        );
        assert_eq!(
            to_s(InterfaceFilter::new("baz", Some(InterfaceKind::V6))),
            "baz:v6"
        );
        assert_eq!(to_s(InterfaceFilter::new("", None)), "");
        assert_eq!(
            to_s(InterfaceFilter::new("", Some(InterfaceKind::V4))),
            ":v4"
        );
        assert_eq!(
            to_s(InterfaceFilter::new("", Some(InterfaceKind::V6))),
            ":v6"
        );
    }

    #[test]
    fn interface_filter_de() {
        let from_s =
            |s: &str| match serde_json::from_value::<InterfaceFilter>(Value::String(s.into())) {
                Ok(x) => x,
                Err(e) => panic!("input: {}, error: {}", s, e),
            };

        assert_eq!(from_s("foo"), InterfaceFilter::new("foo", None));
        assert_eq!(
            from_s("bar:v4"),
            InterfaceFilter::new("bar", Some(InterfaceKind::V4))
        );
        assert_eq!(
            from_s("baz:v6"),
            InterfaceFilter::new("baz", Some(InterfaceKind::V6))
        );
        assert_eq!(from_s(""), InterfaceFilter::new("", None));
        assert_eq!(
            from_s(":v4"),
            InterfaceFilter::new("", Some(InterfaceKind::V4))
        );
        assert_eq!(
            from_s(":v6"),
            InterfaceFilter::new("", Some(InterfaceKind::V6))
        );
    }
}
