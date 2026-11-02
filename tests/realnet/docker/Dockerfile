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
RUN for u in tester fil noisy loud flood; do \
      useradd -m -s /bin/bash "$u" && usermod -p '*' "$u"; \
    done \
 && install -d -o tester -g tester -m 700 /home/tester/.ssh \
 && printf '%s\n' 'echo "NOISY-BASHRC: welcome back"' \
        "printf '\\033[1mbold banner\\033[0m\\r\\n'" > /home/noisy/.bashrc \
 && printf '%s\n' "head -c 60000 /dev/zero | tr '\\0' L" > /home/loud/.bashrc \
 && printf '%s\n' "head -c 100000 /dev/zero | tr '\\0' F" > /home/flood/.bashrc \
 && chown noisy:noisy /home/noisy/.bashrc && chown loud:loud /home/loud/.bashrc \
 && chown flood:flood /home/flood/.bashrc

COPY sshd_config /etc/ssh/sshd_config
COPY banner /etc/fil-realnet/banner
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
# In /usr/bin so they are on the default PATH of every ssh command, whatever sshd sets.
COPY bin/pipe-echo bin/pipe-probe bin/quic-echo /usr/bin/
RUN chmod 755 /usr/local/bin/entrypoint.sh

CMD ["/usr/local/bin/entrypoint.sh"]
