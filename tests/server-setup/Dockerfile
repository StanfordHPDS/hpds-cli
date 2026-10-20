# A stand-in for a freshly created Ubuntu 24.04 cloud VM: systemd as PID 1,
# a passwordless-sudo login user, and nothing else the server profile
# installs.
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends systemd systemd-sysv dbus curl ca-certificates sudo git \
 && rm -rf /var/lib/apt/lists/* \
 && useradd -m -s /bin/bash nero \
 && echo 'nero ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/nero \
 && systemctl mask systemd-logind.service getty.target console-getty.service systemd-udevd.service
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
