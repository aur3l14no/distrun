FROM debian:bookworm-slim

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        coreutils \
        openssh-server \
        procps \
        tmux \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -s /bin/bash distrun \
    && echo 'distrun:disabled' | chpasswd \
    && mkdir -p /run/sshd /home/distrun/.ssh \
    && chmod 700 /home/distrun/.ssh \
    && chown -R distrun:distrun /home/distrun/.ssh \
    && sed -ri 's/^#?PermitRootLogin .*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config \
    && sed -ri 's/^#?PasswordAuthentication .*/PasswordAuthentication no/' /etc/ssh/sshd_config \
    && sed -ri 's/^#?PubkeyAuthentication .*/PubkeyAuthentication yes/' /etc/ssh/sshd_config

EXPOSE 22

CMD ["/usr/sbin/sshd", "-D", "-e"]
