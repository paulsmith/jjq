#!/bin/sh
# ABOUTME: Installs jjq binary and man page to PREFIX (default /usr/local).
# ABOUTME: Included in release tarballs for easy installation.
set -eu

PREFIX="${PREFIX:-/usr/local}"

cd "$(dirname "$0")"

install -d "$PREFIX/bin" "$PREFIX/share/man/man1"
install -m 755 bin/jjq "$PREFIX/bin/jjq"
install -m 644 share/man/man1/jjq.1.gz "$PREFIX/share/man/man1/jjq.1.gz"

echo "Installed jjq to $PREFIX"
