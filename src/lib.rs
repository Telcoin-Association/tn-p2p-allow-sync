//! Committee allowlist for the P2P ports of a Telcoin Network node.
//!
//! `tn-p2p-allow-sync` reads the node's admission snapshot over loopback JSON-RPC, validates it, renders one nftables program for the `inet tn_p2p_allow` table and loads it in a single transaction.
//! A run that cannot get an authoritative snapshot keeps the last good program loaded and says so through its exit status.
//!
//! The binary only forwards its arguments to [`cli::run`].
//! Everything else lives in this library, so tests call it directly.

pub mod addr;
pub mod cli;
pub mod render;
