
# Hosts commonly send LANG/LC_* over SSH (Ubuntu's ssh_config has
# `SendEnv LANG LC_*`) and the guest's sshd accepts every variable, so a host
# locale the guest lacks makes every shell print setlocale warnings.
# en_US.UTF-8 is by far the most common host locale; C.UTF-8 ships with libc.
echo '  [guest] Generating the en_US.UTF-8 locale...'
locale-gen en_US.UTF-8
