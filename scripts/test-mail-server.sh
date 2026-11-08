#!/bin/sh
# Local mail server for testing (GreenMail in podman), with its own throwaway
# certificate authority so TLS is real and verified. Never uses real accounts.
#
#   scripts/test-mail-server.sh start   # prints the env var to trust the test CA
#   scripts/test-mail-server.sh stop
#
# Users: alice@carteiro.test / alice-pw, bob@carteiro.test / bob-pw
# Ports (localhost only): SMTP 3025 (STARTTLS), SMTPS 3465, IMAP 3143 (STARTTLS),
# IMAPS 3993, POP3 3110 (STLS), POP3S 3995.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="$ROOT/.testmail"
NAME=carteiro-testmail
IMAGE=docker.io/greenmail/standalone@sha256:45be11722392e26ab2ef9cb339e6c13b27e3fff9e6655abbfba5f7de75408d51

case "${1:-start}" in
stop)
  podman rm -f "$NAME" >/dev/null 2>&1 || true
  echo "stopped"; exit 0 ;;
start) ;;
*) echo "usage: $0 start|stop" >&2; exit 2 ;;
esac

mkdir -p "$DIR"
if [ ! -f "$DIR/ca.pem" ]; then
  openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=Carteiro Test CA" \
    -keyout "$DIR/ca.key" -out "$DIR/ca.pem" 2>/dev/null
  openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" \
    -keyout "$DIR/server.key" -out "$DIR/server.csr" 2>/dev/null
  # 10.0.2.2 is the Android emulator's address for this machine's 127.0.0.1.
  printf "subjectAltName=DNS:localhost,IP:127.0.0.1,IP:10.0.2.2\nextendedKeyUsage=serverAuth\n" > "$DIR/ext.cnf"
  openssl x509 -req -in "$DIR/server.csr" -CA "$DIR/ca.pem" -CAkey "$DIR/ca.key" -CAcreateserial \
    -days 30 -extfile "$DIR/ext.cnf" -out "$DIR/server.pem" 2>/dev/null
  openssl pkcs12 -export -inkey "$DIR/server.key" -in "$DIR/server.pem" -certfile "$DIR/ca.pem" \
    -name greenmail -passout pass:changeit -out "$DIR/server.p12"
  chmod 644 "$DIR/server.p12"
fi

podman rm -f "$NAME" >/dev/null 2>&1 || true
podman run -d --name "$NAME" \
  -p 127.0.0.1:3025:3025 -p 127.0.0.1:3465:3465 -p 127.0.0.1:3143:3143 \
  -p 127.0.0.1:3993:3993 -p 127.0.0.1:3110:3110 -p 127.0.0.1:3995:3995 \
  -v "$DIR:/certs:ro,Z" \
  -e GREENMAIL_OPTS="-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 \
-Dgreenmail.users=alice:alice-pw@carteiro.test,bob:bob-pw@carteiro.test -Dgreenmail.users.login=email \
-Dgreenmail.tls.keystore.file=/certs/server.p12 -Dgreenmail.tls.keystore.password=changeit \
-Dgreenmail.verbose" \
  "$IMAGE" >/dev/null

# Wait until IMAPS answers.
for _ in $(seq 1 30); do
  if openssl s_client -connect 127.0.0.1:3993 -CAfile "$DIR/ca.pem" -verify_return_error </dev/null 2>/dev/null | grep -q "OK"; then
    echo "Test mail server is up."
    echo "export CARTEIRO_TEST_CA=$DIR/ca.pem"
    exit 0
  fi
  sleep 1
done
echo "server did not start; logs:" >&2
podman logs "$NAME" 2>&1 | tail -20 >&2
exit 1
