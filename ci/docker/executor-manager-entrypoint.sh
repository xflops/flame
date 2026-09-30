#!/bin/sh
# CI-only identity: each Compose replica uses its hostname as its system node name.
set -eu

node_name="$(hostname)"
identity_dir="/tmp/flame-identity"
mkdir -p "$identity_dir"
openssl genrsa -out "$identity_dir/executor.key" 2048
openssl req -new -key "$identity_dir/executor.key" -out "$identity_dir/executor.csr" \
    -subj "/CN=$node_name/O=Flame"
cat > "$identity_dir/executor.ext" << EOF
basicConstraints=CA:FALSE
keyUsage=digitalSignature
extendedKeyUsage=clientAuth
subjectAltName=URI:spiffe://flame.local/flame/system/node/$node_name
EOF
openssl x509 -req -in "$identity_dir/executor.csr" \
    -CA /etc/flame/certs/ca.crt -CAkey /etc/flame/certs/ca.key \
    -set_serial "0x$(openssl rand -hex 16)" -out "$identity_dir/executor.crt" \
    -days 1 -extfile "$identity_dir/executor.ext"
chmod 600 "$identity_dir/executor.key"
sed \
    -e "s|/etc/flame/certs/server.crt|$identity_dir/executor.crt|g" \
    -e "s|/etc/flame/certs/server.key|$identity_dir/executor.key|g" \
    /root/.flame/flame-cluster.yaml > "$identity_dir/flame-cluster.yaml"
export FLAME_CERT_FILE=/etc/flame/certs/admin.crt
export FLAME_KEY_FILE=/etc/flame/certs/admin.key
exec /usr/local/flame/bin/flame-executor-manager --config "$identity_dir/flame-cluster.yaml"
