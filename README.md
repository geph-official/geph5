# Geph5

Geph5 is a major rewrite with a few big architectural differences from Geph4 that largely serve the goal of **simplifying and massively cleaning up the design**:

## Overview

- `sosistab2`, or anything similar that builds a reliable transport on unreliable pipes, is no longer used. Instead, obfuscated transports must themselves provide reliable transport. In practice, this means that stuff is based on streams multiplexed over TCP, not packets and UDP.
- The client no longer has complex logic for intelligently hot-swapping pipes. This has proven to be difficult to achieve given diverse network environments, extremely inaccurate/sleepy phone clocks, etc. Instead, a session is started and used until it breaks, and another session is started, etc. With fast enough session creation, the only noticeable difference is that proxied TCP connections reset, which most applications handle gracefully.
- The central authentication server is called the **broker**, not the binder. It also now uses a simple JSON-RPC API without end-to-end encryption, with integrity-critical responses having ed25519 signatures.
  - Instead of an implementation based on RSA blind signatures with a vast number of keys, mizaru2 will use some scheme that embeds a few unblinded bits in each anonymous credential, as well as exit-side noninteractive verification. The latter will _greatly_ reduce load on the broker as well as connection latency, since exits no longer need to communicate with the broker for every new session.
- The broker is in charge of communicating with bridges and exits to set up routes for users. Complex `(number of bridges) * (number of exits)` communication patterns are eliminated, and the broker can be easily used to generate routes for Earendil and similar software.
- VPN mode is supported by tunneling through stream ("socks5") mode, but with support for intercepting traffic tun2socks-style instead.
- We pervasively use config files rather than massive strings of command-line arguments.
- We no longer use webviews for GUI. Instead, GUI clients are written in Rust and directly call protocol libraries.

## License

The code is generally licensed under **MPL 2.0**. Low-level libraries useful to a wide variety of projects, such as the `sillad` framework, are generally licensed under the ISC license.

## Code organization

Unlike Geph4, Geph5 is organized in a Cargo workspace, "monorepo"-style:

- `libraries/` contains library crates that may depend on each other. All of these crates also receive crates.io releases.
- `binaries/` contains binary crates.
  - `geph5-client`
  - `geph5-exit`
  - `geph5-bridge`
  - `geph5-broker`
