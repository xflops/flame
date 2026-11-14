"""Read and validate client TLS credentials without writing inline secrets to disk."""

import base64
import binascii
import re
from pathlib import Path
from typing import TYPE_CHECKING, Optional

import grpc

if TYPE_CHECKING:
    from flamepy.core.types import FlameClientTls

_PEM_BLOCK = re.compile(rb"-----BEGIN ([A-Z0-9 ]+)-----.*?-----END \1-----", re.DOTALL)
_KEY_LABELS = {b"PRIVATE KEY", b"RSA PRIVATE KEY", b"EC PRIVATE KEY"}


def _credential(config: "FlameClientTls", name: str) -> tuple[Optional[bytes], str]:
    """Read one file or decode one canonical base64 source with safe diagnostics."""
    inline = getattr(config, f"{name}_data")
    field = f"{name}_data" if inline is not None else f"{name}_file"
    if inline is not None:
        try:
            decoded = base64.b64decode(inline, validate=True)
            if base64.b64encode(decoded).decode("ascii") != inline:
                raise ValueError("noncanonical base64")
        except (ValueError, binascii.Error):
            raise ValueError(f"TLS {field} must contain canonical standard base64-encoded PEM") from None
        return decoded, field
    path = getattr(config, field)
    if path is None:
        return None, field
    try:
        return Path(path).read_bytes(), field
    except (OSError, ValueError):
        raise ValueError(f"TLS {field} could not be read") from None


def _pem_blocks(data: bytes, labels: set[bytes], field: str) -> list[bytes]:
    """Extract relevant PEM blocks, retaining support for annotated bundles."""
    try:
        data.decode("utf-8")
    except UnicodeDecodeError:
        raise ValueError(f"TLS {field} must contain UTF-8 PEM material") from None
    blocks = [match.group() for match in _PEM_BLOCK.finditer(data) if match.group(1) in labels]
    starts = sum(data.count(b"-----BEGIN " + label + b"-----") for label in labels)
    if not blocks or starts != len(blocks):
        raise ValueError(f"TLS {field} contains invalid PEM material")
    return blocks


def _certificates(data: bytes, field: str):
    """Parse every certificate in a PEM chain without exposing parser input."""
    from cryptography import x509
    from cryptography.exceptions import UnsupportedAlgorithm

    blocks = _pem_blocks(data, {b"CERTIFICATE"}, field)
    try:
        return [x509.load_pem_x509_certificate(block) for block in blocks]
    except (ValueError, UnsupportedAlgorithm):
        raise ValueError(f"TLS {field} must contain valid PEM certificates") from None


def _private_key(data: bytes, field: str):
    """Parse exactly one unencrypted PEM key using the in-memory crypto loader."""
    from cryptography.exceptions import UnsupportedAlgorithm
    from cryptography.hazmat.primitives import serialization

    blocks = _pem_blocks(data, _KEY_LABELS, field)
    if len(blocks) != 1:
        raise ValueError(f"TLS {field} must contain exactly one unencrypted PEM private key")
    try:
        return serialization.load_pem_private_key(blocks[0], password=None)
    except (ValueError, TypeError, UnsupportedAlgorithm):
        raise ValueError(f"TLS {field} must contain a valid unencrypted PEM private key") from None


def channel_credentials(config: Optional["FlameClientTls"]) -> grpc.ChannelCredentials:
    """Build verified gRPC credentials from validated in-memory PEM material."""
    if config is None:
        return grpc.ssl_channel_credentials()
    config.validate_identity()
    roots, ca_field = _credential(config, "ca")
    certificate_chain, cert_field = _credential(config, "cert")
    private_key, key_field = _credential(config, "key")
    if roots is not None:
        from cryptography.hazmat.primitives import serialization

        roots = b"".join(certificate.public_bytes(serialization.Encoding.PEM) for certificate in _certificates(roots, ca_field))
    if certificate_chain is not None:
        from cryptography.exceptions import UnsupportedAlgorithm
        from cryptography.hazmat.primitives import serialization

        certificates = _certificates(certificate_chain, cert_field)
        key = _private_key(private_key, key_field)
        encoding = serialization.Encoding.DER
        public_format = serialization.PublicFormat.SubjectPublicKeyInfo
        try:
            certificate_public = certificates[0].public_key().public_bytes(encoding, public_format)
            private_public = key.public_key().public_bytes(encoding, public_format)
        except (ValueError, UnsupportedAlgorithm):
            raise ValueError("TLS client identity contains invalid or unsupported public key material") from None
        if certificate_public != private_public:
            raise ValueError("TLS client certificate and private key do not match")
        certificate_chain = b"".join(certificate.public_bytes(serialization.Encoding.PEM) for certificate in certificates)
        private_key = key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption())
    return grpc.ssl_channel_credentials(root_certificates=roots, private_key=private_key, certificate_chain=certificate_chain)
