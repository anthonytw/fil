# One image for every host in the SSH topology: client, jump1, jump2 and target.
# Nothing secret is baked in. Keys and host keys are made at run time (see ssh.sh, entrypoint.sh).
FROM ubuntu:24.04

RUN apt-get update \
 && apt-get install -y --no-install-recommends openssh-server openssh-client procps iproute2 \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /run/sshd /etc/fil-realnet

# Users. Passwords are disabled ('*' still allows public-key logins, '!' would lock the account).
#   tester: runs the ssh client in the client container
#   fil:    the clean account (no shell output before the program starts)
#   noisy:  .bashrc prints text on every non-interactive login
#   loud:   .bashrc prints 60000 bytes, just under the 64 KiB preamble bound
#   flood:  .bashrc prints 100000 bytes, over the bound
#   mallory: a second, ordinary user (the peer-UID and directory checks, and "another user's daemon")
#   victim, victim2, victim3: users whose socket directory somebody else prepared (f_other_user)
RUN for u in tester fil noisy loud flood mallory victim victim2 victim3; do \
      useradd -m -s /bin/bash "$u" && usermod -p '*' "$u"; \
    done \
 && install -d -o tester -g tester -m 700 /home/tester/.ssh \
 && printf '%s\n' 'echo "NOISY-BASHRC: welcome back"' \
        "printf '\\033[1mbold banner\\033[0m\\r\\n'" > /home/noisy/.bashrc \
 && printf '%s\n' "head -c 60000 /dev/zero | tr '\\0' L" > /home/loud/.bashrc \
 && printf '%s\n' "head -c 100000 /dev/zero | tr '\\0' F" > /home/flood/.bashrc \
 && chown noisy:noisy /home/noisy/.bashrc && chown loud:loud /home/loud/.bashrc \
 && chown flood:flood /home/flood/.bashrc

# The real fil client and daemon read these. No direct mode here (the networks are internal, so
# there is no UDP path), and the daemon never retires on its own so scenarios cannot race it.
RUN for u in fil noisy mallory; do \
      install -d -o "$u" -g "$u" -m 700 "/home/$u/.config" "/home/$u/.config/fil" \
 && printf 'direct = false\nidle_exit_secs = 0\n' > "/home/$u/.config/fil/daemon.toml" \
 && chown "$u:$u" "/home/$u/.config/fil/daemon.toml"; \
    done \
 && install -d -o tester -g tester -m 700 /home/tester/.config /home/tester/.config/fil \
 && printf '[defaults]\ntransport = "pipe"\n' > /home/tester/.config/fil/config.toml \
 && chown tester:tester /home/tester/.config/fil/config.toml

COPY sshd_config /etc/ssh/sshd_config
COPY banner /etc/fil-realnet/banner
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
# In /usr/bin so they are on the default PATH of every ssh command, whatever sshd sets.
COPY bin/pipe-echo bin/pipe-probe bin/quic-echo bin/fil bin/ptydrive bin/sockprobe /usr/bin/
RUN chmod 755 /usr/local/bin/entrypoint.sh

CMD ["/usr/local/bin/entrypoint.sh"]
