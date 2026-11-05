//! The nft program, schema 1: the one text `nft -f` loads for the `inet tn_p2p_allow` table.
//!
//! The text is a contract shared with the deployment scripts, and the host-metrics collector sums the two `tn_` counters by name.
//! One program declares the table, flushes it and its four sets, adds the input chain and adds the elements, and `nft -f` applies the whole file as one transaction.
//!
//! Nothing in the text comes from the clock, the epoch or a count.
//! The only inputs are std-formatted IPs, CIDRs and ports, taken in the numeric order of their sets, so equal sets give byte-equal programs.

use std::collections::BTreeSet;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

use crate::addr::Cidr;

/// The first line of every program, without its newline.
///
/// A file whose first line equals this was written by [`render`] for schema 1.
pub const HEADER: &str = "# firewall-setup.sh: P2P committee allowlist, schema 1; loaded whole by nft -f (one transaction).";

/// The table declaration and the flushes that follow the header; nothing in them depends on the input.
///
/// Declaring before flushing lets the flush succeed on a host where the table does not exist yet.
const DECLARE_AND_FLUSH: &str = "\
table inet tn_p2p_allow {
    set allow_v4 { type ipv4_addr; }
    set allow_v6 { type ipv6_addr; }
    set static_v4 { type ipv4_addr; flags interval; }
    set static_v6 { type ipv6_addr; flags interval; }
    counter tn_accept_p2p_listed { }
    counter tn_drop_p2p_unlisted { }
}
flush table inet tn_p2p_allow
flush set inet tn_p2p_allow allow_v4
flush set inet tn_p2p_allow allow_v6
flush set inet tn_p2p_allow static_v4
flush set inet tn_p2p_allow static_v6
";

/// The accept rules in chain order: the address family keyword and the set each one matches.
const ACCEPT_RULES: [(&str, &str); 4] = [
    ("ip", "allow_v4"),
    ("ip", "static_v4"),
    ("ip6", "allow_v6"),
    ("ip6", "static_v6"),
];

/// The inputs of one program: the P2P ports and the four sets.
///
/// The sets are taken as given.
/// The allow sets hold addresses already passed through [`crate::addr::canonical_ip`].
/// The static sets hold masked [`Cidr`] values split by family: IPv4 networks in `static_v4`, IPv6 networks in `static_v6`.
///
/// `Display` writes the same text as [`render`].
#[derive(Debug, Clone, Copy)]
pub struct Program<'a> {
    /// The UDP ports the rules match; the caller refuses an empty set, whose port list nft rejects as a syntax error.
    pub ports: &'a BTreeSet<u16>,
    /// Single IPv4 addresses allowed on the ports.
    pub allow_v4: &'a BTreeSet<Ipv4Addr>,
    /// Single IPv6 addresses allowed on the ports.
    pub allow_v6: &'a BTreeSet<Ipv6Addr>,
    /// IPv4 networks allowed on the ports, from the static list.
    pub static_v4: &'a BTreeSet<Cidr>,
    /// IPv6 networks allowed on the ports, from the static list.
    pub static_v6: &'a BTreeSet<Cidr>,
}

/// Renders the program for `p`: the header, the declarations and flushes, the input chain, then one `add element` block per non-empty set.
///
/// The element blocks come in the order allow_v4, allow_v6, static_v4, static_v6.
/// A set with one element is written inline as `{ X }`; a larger set gets one element per line, indented by four spaces, with a comma after every element but the last.
/// The ports are always written as `{ a, b }`, braces included for a single port.
/// The text ends with a newline.
pub fn render(p: &Program<'_>) -> String {
    p.to_string()
}

impl fmt::Display for Program<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ports = Ports(self.ports);
        writeln!(f, "{HEADER}")?;
        f.write_str(DECLARE_AND_FLUSH)?;
        f.write_str("table inet tn_p2p_allow {\n")?;
        f.write_str("    chain input {\n")?;
        f.write_str("        type filter hook input priority -20; policy accept;\n")?;
        for (family, set) in ACCEPT_RULES {
            writeln!(
                f,
                "        udp dport {ports} {family} saddr @{set} counter name \"tn_accept_p2p_listed\" accept"
            )?;
        }
        writeln!(
            f,
            "        udp dport {ports} counter name \"tn_drop_p2p_unlisted\" drop"
        )?;
        f.write_str("    }\n")?;
        f.write_str("}\n")?;
        write_elements(f, "allow_v4", self.allow_v4)?;
        write_elements(f, "allow_v6", self.allow_v6)?;
        write_elements(f, "static_v4", self.static_v4)?;
        write_elements(f, "static_v6", self.static_v6)
    }
}

/// The port list of a rule: `{ a, b }` in ascending order.
struct Ports<'a>(&'a BTreeSet<u16>);

impl fmt::Display for Ports<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{ ")?;
        for (i, port) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{port}")?;
        }
        f.write_str(" }")
    }
}

/// Writes the `add element` block of `set`, or nothing when `items` is empty.
fn write_elements<T: fmt::Display>(
    f: &mut fmt::Formatter<'_>,
    set: &str,
    items: &BTreeSet<T>,
) -> fmt::Result {
    let mut iter = items.iter();
    let Some(first) = iter.next() else {
        return Ok(());
    };
    if items.len() == 1 {
        return writeln!(f, "add element inet tn_p2p_allow {set} {{ {first} }}");
    }
    writeln!(f, "add element inet tn_p2p_allow {set} {{")?;
    write!(f, "    {first}")?;
    for item in iter {
        write!(f, ",\n    {item}")?;
    }
    f.write_str("\n}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::net::IpAddr;

    type TestResult = Result<(), Box<dyn Error>>;

    /// Owned inputs, so a test can borrow them into a [`Program`].
    #[derive(Default)]
    struct Sets {
        ports: BTreeSet<u16>,
        allow_v4: BTreeSet<Ipv4Addr>,
        allow_v6: BTreeSet<Ipv6Addr>,
        static_v4: BTreeSet<Cidr>,
        static_v6: BTreeSet<Cidr>,
    }

    impl Sets {
        fn program(&self) -> Program<'_> {
            Program {
                ports: &self.ports,
                allow_v4: &self.allow_v4,
                allow_v6: &self.allow_v6,
                static_v4: &self.static_v4,
                static_v6: &self.static_v6,
            }
        }
    }

    /// The lines of `text` from the first `add element` line on.
    fn element_lines(text: &str) -> Vec<&str> {
        text.lines()
            .skip_while(|line| !line.starts_with("add element"))
            .collect()
    }

    #[test]
    fn header_is_the_first_line_and_the_text_ends_with_a_newline() {
        let sets = Sets::default();
        let text = render(&sets.program());
        assert_eq!(text.lines().next(), Some(HEADER));
        assert!(text.ends_with("}\n"), "{text}");
        assert!(!text.ends_with("\n\n"), "{text}");
    }

    #[test]
    fn display_writes_the_rendered_text() -> TestResult {
        let sets = Sets {
            ports: BTreeSet::from([49590]),
            allow_v4: BTreeSet::from(["192.0.2.10".parse()?]),
            ..Sets::default()
        };
        let program = sets.program();
        assert_eq!(format!("{program}"), render(&program));
        Ok(())
    }

    #[test]
    fn ports_sort_numerically_inside_braces() {
        assert_eq!(
            Ports(&BTreeSet::from([49594, 9000, 49590])).to_string(),
            "{ 9000, 49590, 49594 }"
        );
        assert_eq!(Ports(&BTreeSet::from([49590])).to_string(), "{ 49590 }");
    }

    #[test]
    fn every_rule_carries_the_same_port_list() {
        let sets = Sets {
            ports: BTreeSet::from([49594, 9000]),
            ..Sets::default()
        };
        let text = render(&sets.program());
        let rules: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("udp dport"))
            .collect();
        assert_eq!(rules.len(), 5, "{text}");
        for rule in rules {
            assert!(
                rule.starts_with("        udp dport { 9000, 49594 } "),
                "{rule}"
            );
        }
    }

    #[test]
    fn an_empty_set_writes_no_element_block() {
        let sets = Sets {
            ports: BTreeSet::from([49590]),
            ..Sets::default()
        };
        let text = render(&sets.program());
        assert!(!text.contains("add element"), "{text}");
    }

    #[test]
    fn one_element_is_inline_and_more_are_one_per_line() -> TestResult {
        let sets = Sets {
            ports: BTreeSet::from([49590]),
            allow_v4: BTreeSet::from(["192.0.2.10".parse()?]),
            allow_v6: BTreeSet::from(["2001:db8::9".parse()?, "2001:db8::10".parse()?]),
            static_v4: BTreeSet::from([
                Cidr::new(IpAddr::V4("10.20.0.0".parse()?), 16).ok_or("prefix")?,
                Cidr::new(IpAddr::V4("10.3.0.0".parse()?), 16).ok_or("prefix")?,
                Cidr::new(IpAddr::V4("192.0.2.0".parse()?), 25).ok_or("prefix")?,
            ]),
            ..Sets::default()
        };
        let text = render(&sets.program());
        assert_eq!(
            element_lines(&text),
            [
                "add element inet tn_p2p_allow allow_v4 { 192.0.2.10 }",
                "add element inet tn_p2p_allow allow_v6 {",
                "    2001:db8::9,",
                "    2001:db8::10",
                "}",
                "add element inet tn_p2p_allow static_v4 {",
                "    10.3.0.0/16,",
                "    10.20.0.0/16,",
                "    192.0.2.0/25",
                "}",
            ]
        );
        Ok(())
    }

    #[test]
    fn element_blocks_follow_the_set_order_not_the_rule_order() -> TestResult {
        let sets = Sets {
            ports: BTreeSet::from([49590]),
            allow_v4: BTreeSet::from(["192.0.2.10".parse()?]),
            allow_v6: BTreeSet::from(["2001:db8::7".parse()?]),
            static_v4: BTreeSet::from([
                Cidr::new(IpAddr::V4("203.0.113.0".parse()?), 24).ok_or("prefix")?
            ]),
            static_v6: BTreeSet::from([
                Cidr::new(IpAddr::V6("2001:db8:100::".parse()?), 48).ok_or("prefix")?
            ]),
        };
        let text = render(&sets.program());
        let blocks: Vec<&str> = element_lines(&text)
            .into_iter()
            .filter_map(|line| line.strip_prefix("add element inet tn_p2p_allow "))
            .filter_map(|rest| rest.split(' ').next())
            .collect();
        assert_eq!(blocks, ["allow_v4", "allow_v6", "static_v4", "static_v6"]);
        Ok(())
    }
}
