# Reqwest 0.13.4 request-extension forwarding

This directory contains the published `reqwest` 0.13.4 crate from crates.io.
The upstream repository is <https://github.com/seanmonstar/reqwest>, revision
`11489b34eda6d32b15ad4033e62beba2ee401350` (see `.cargo_vcs_info.json`).
The registry package checksum is
`219c5811de6525e5416c7d5d53bb656d3afdbc6c5af816e0802bcfa42dbdc1c3`.
The upstream MIT and Apache-2.0 license files are retained unchanged.

The only functional change is the two lines in
`src/async_impl/client.rs` that forward request extensions to the underlying
HTTP request, for both HTTP/1–2 and the optional HTTP/3 branch. The isolated
diff is in `request-extensions.patch`. No timeout, connector, proxy, TLS,
DNS, redirect, or non-HLS request behavior is replaced.

HLS attaches Hyper-Util 0.1.20's request-local `capture_connection` extension
through Reqwest's public HTTP request conversions. This provides the signal
needed to start a per-attempt origin-transfer deadline after connecting,
including pooled connections. Requests without this extension keep the
existing behavior.

The root `[patch.crates-io]` selects this local pinned source. The crate is
excluded from workspace formatting and linting; upstream sources are not
reformatted. Both build Dockerfiles copy it into their workspace skeleton.
