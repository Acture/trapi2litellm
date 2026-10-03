ARG BASE=debian:13
FROM ${BASE}
RUN ["apt-get", "update"]
RUN ["apt-get", "install", "-y", "--no-install-recommends", "python3", "python3-venv", "ca-certificates", "dpkg-dev", "binutils", "libstdc++6", "libgcc-s1", "libgomp1", "systemd", "systemd-sysv", "dbus", "dbus-user-session", "procps", "passwd", "login"]
COPY --from=ghcr.io/astral-sh/uv:0.12.17 /uv /usr/local/bin/uv
WORKDIR /src
