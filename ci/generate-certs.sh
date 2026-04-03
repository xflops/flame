#!/bin/bash
#
# Generate self-signed TLS certificates for Flame CI/development
#
# Usage: ./generate-certs.sh [options]
#   -o, --output DIR       Directory to output certificates (default: ./certs)
#   -s, --san-list LIST    Comma-separated list of SANs (default: localhost,127.0.0.1)
#   -i, --ip-range CIDR    IP range in CIDR notation to add as SANs (e.g., 172.20.0.0/24)
#                          Only supports /24 networks, adds .1 through .20 as SANs
#   -h, --help             Show this help message
#
# Output files:
#   ca.crt       - CA certificate
#   ca.key       - CA private key
#   server.crt   - Server certificate (signed by CA)
#   server.key   - Server private key
#   client.crt   - Client certificate for mTLS (signed by CA)
#   client.key   - Client private key

set -e

# Default values
OUTPUT_DIR="./certs"
SAN_LIST="localhost,127.0.0.1"
IP_RANGE=""
VALID_DAYS=365

# Parse command line arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        -o|--output)
            OUTPUT_DIR="$2"
            shift 2
            ;;
        -s|--san-list)
            SAN_LIST="$2"
            shift 2
            ;;
        -i|--ip-range)
            IP_RANGE="$2"
            shift 2
            ;;
        -h|--help)
            echo "Usage: $0 [options]"
            echo ""
            echo "Options:"
            echo "  -o, --output DIR       Directory to output certificates (default: ./certs)"
            echo "  -s, --san-list LIST    Comma-separated list of SANs (default: localhost,127.0.0.1)"
            echo "  -i, --ip-range CIDR    IP range in CIDR notation (e.g., 172.20.0.0/24)"
            echo "  -h, --help             Show this help message"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            echo "Use --help for usage information"
            exit 1
            ;;
    esac
done

echo "🔐 Generating TLS certificates..."
echo "   Output directory: $OUTPUT_DIR"
echo "   SANs: $SAN_LIST"
if [ -n "$IP_RANGE" ]; then
    echo "   IP Range: $IP_RANGE"
fi
echo ""

# Create output directory
mkdir -p "$OUTPUT_DIR"

# Build SAN extension string for openssl
SAN_EXT="subjectAltName = "
IFS=',' read -ra SANS <<< "$SAN_LIST"
FIRST=true
for san in "${SANS[@]}"; do
    san=$(echo "$san" | xargs)  # trim whitespace
    if [ "$FIRST" = true ]; then
        FIRST=false
    else
        SAN_EXT+=","
    fi
    # Check if it's an IP address
    if [[ "$san" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        SAN_EXT+="IP:$san"
    else
        SAN_EXT+="DNS:$san"
    fi
done

# Add IP range if specified (supports /24 CIDR notation)
if [ -n "$IP_RANGE" ]; then
    # Extract base IP (e.g., 172.20.0 from 172.20.0.0/24)
    BASE_IP=$(echo "$IP_RANGE" | sed 's|/.*||' | sed 's|\.[0-9]*$||')
    echo "→ Adding IPs from range $IP_RANGE (${BASE_IP}.1 - ${BASE_IP}.20)..."
    for i in $(seq 1 20); do
        SAN_EXT+=",IP:${BASE_IP}.${i}"
    done
fi

# Generate CA private key
echo "→ Generating CA private key..."
openssl genrsa -out "$OUTPUT_DIR/ca.key" 4096

# Generate CA certificate
echo "→ Generating CA certificate..."
openssl req -new -x509 -days $VALID_DAYS -key "$OUTPUT_DIR/ca.key" \
    -out "$OUTPUT_DIR/ca.crt" \
    -subj "/CN=Flame CA/O=Flame"

# Generate server private key
echo "→ Generating server private key..."
openssl genrsa -out "$OUTPUT_DIR/server.key" 4096

# Generate server CSR
echo "→ Generating server CSR..."
openssl req -new -key "$OUTPUT_DIR/server.key" \
    -out "$OUTPUT_DIR/server.csr" \
    -subj "/CN=flame-server/O=Flame"

# Create extensions file for server SAN
cat > "$OUTPUT_DIR/server.ext" << EOF
authorityKeyIdentifier=keyid,issuer
basicConstraints=CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
$SAN_EXT
EOF

# Sign server certificate with CA
echo "→ Signing server certificate with CA..."
openssl x509 -req -in "$OUTPUT_DIR/server.csr" \
    -CA "$OUTPUT_DIR/ca.crt" -CAkey "$OUTPUT_DIR/ca.key" \
    -CAcreateserial -out "$OUTPUT_DIR/server.crt" \
    -days $VALID_DAYS -extfile "$OUTPUT_DIR/server.ext"

# Generate client private key (for mTLS)
echo "→ Generating client private key..."
openssl genrsa -out "$OUTPUT_DIR/client.key" 4096

# Generate client CSR
echo "→ Generating client CSR..."
openssl req -new -key "$OUTPUT_DIR/client.key" \
    -out "$OUTPUT_DIR/client.csr" \
    -subj "/CN=flame-client/O=Flame"

# Create extensions file for client cert
cat > "$OUTPUT_DIR/client.ext" << EOF
authorityKeyIdentifier=keyid,issuer
basicConstraints=CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = clientAuth
EOF

# Sign client certificate with CA
echo "→ Signing client certificate with CA..."
openssl x509 -req -in "$OUTPUT_DIR/client.csr" \
    -CA "$OUTPUT_DIR/ca.crt" -CAkey "$OUTPUT_DIR/ca.key" \
    -CAcreateserial -out "$OUTPUT_DIR/client.crt" \
    -days $VALID_DAYS -extfile "$OUTPUT_DIR/client.ext"

# Clean up temporary files
rm -f "$OUTPUT_DIR/server.csr" "$OUTPUT_DIR/server.ext" \
      "$OUTPUT_DIR/client.csr" "$OUTPUT_DIR/client.ext" \
      "$OUTPUT_DIR/ca.srl"

# Set restrictive permissions on private keys
chmod 600 "$OUTPUT_DIR/ca.key" "$OUTPUT_DIR/server.key" "$OUTPUT_DIR/client.key"

echo ""
echo "✓ Generated certificates in $OUTPUT_DIR:"
echo "  - ca.crt      (CA certificate)"
echo "  - ca.key      (CA private key - for signing session certs)"
echo "  - server.crt  (Server certificate)"
echo "  - server.key  (Server private key)"
echo "  - client.crt  (Client certificate for mTLS)"
echo "  - client.key  (Client private key)"
echo ""
echo "Server certificate SANs:"
openssl x509 -in "$OUTPUT_DIR/server.crt" -noout -ext subjectAltName | grep -v "X509v3" || echo "  $SAN_LIST"
