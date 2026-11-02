#!/bin/sh
# Host keys are made here, at container start: nothing is baked into the image.
set -eu
ssh-keygen -A > /dev/null
exec /usr/sbin/sshd -D -E /var/log/sshd.log -f /etc/ssh/sshd_config
