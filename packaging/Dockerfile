ARG BASE=debian:13
FROM rust:1.99.0-bookworm AS rust-toolchain
FROM ${BASE}
RUN ["apt-get", "update"]
RUN ["apt-get", "install", "-y", "--no-install-recommends", "python3", "python3-venv", "ca-certificates", "build-essential", "dpkg-dev", "binutils", "libstdc++6", "libgcc-s1", "libgomp1", "systemd", "systemd-sysv", "dbus", "dbus-user-session", "procps", "passwd", "login"]
COPY --from=rust-toolchain /usr/local/cargo /usr/local/cargo
COPY --from=rust-toolchain /usr/local/rustup /usr/local/rustup
ENV PATH="/usr/local/cargo/bin:${PATH}" RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
COPY --from=ghcr.io/astral-sh/uv:0.12.17 /uv /usr/local/bin/uv
WORKDIR /src
