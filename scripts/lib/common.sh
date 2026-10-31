# Shared by build.sh, setup.sh and sdk.sh. POSIX sh. It is sourced from a checkout and inlined into the
# hosted setup.sh and sdk.sh by scripts/inline.sh, so it must not rely on its own location except to find
# the flmenv.sh.in template (the inlined copy carries the template instead).

FLAME_REPO="${FLAME_REPO:-xflops/flame}"
FLAME_PACKAGE_FORMAT=1
# The release a script installs by default. A hosted script is published with its own release's tag
# (scripts/inline.sh --version TAG replaces the line below); a script run from a checkout has none, and
# then the default is the latest release.
FLAME_DEFAULT_VERSION=""

log() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
need() { have "$1" || die "$1 is required but was not found"; }

# detect_platform sets FLAME_OS and FLAME_ARCH (amd64 or arm64), as release assets are named.
# FLAME_OS and FLAME_ARCH override the detection, for tests and for fetching another platform.
detect_platform() {
    if [ -z "${FLAME_OS:-}" ]; then
        case "$(uname -s)" in
            Linux) FLAME_OS=linux ;;
            *) die "unsupported operating system $(uname -s): Flame releases are built for Linux" ;;
        esac
    fi
    if [ -z "${FLAME_ARCH:-}" ]; then
        case "$(uname -m)" in
            x86_64 | amd64) FLAME_ARCH=amd64 ;;
            aarch64 | arm64) FLAME_ARCH=arm64 ;;
            *) die "unsupported architecture $(uname -m)" ;;
        esac
    fi
}

# resolve_tag TAG: TAG when given, else $FLAME_VERSION, else the release this script was published
# with, else the latest release (the redirect of /releases/latest names the tag). Prints the tag.
resolve_tag() {
    tag="${1:-${FLAME_VERSION:-$FLAME_DEFAULT_VERSION}}"
    if [ -z "$tag" ]; then
        need curl
        url="${FLAME_LATEST_URL:-https://github.com/$FLAME_REPO/releases/latest}"
        final="$(curl -fsSL -o /dev/null -w '%{url_effective}' "$url")" \
            || die "could not find the latest release at $url"
        tag="${final##*/}"
        case "$tag" in
            "" | latest) die "could not find the latest release at $url" ;;
        esac
    fi
    printf '%s' "$tag"
}

# release_base TAG: where the release assets are, $FLAME_BASE_URL when set (a mirror, or a directory).
release_base() {
    printf '%s' "${FLAME_BASE_URL:-https://github.com/$FLAME_REPO/releases/download/$1}"
}

# download URL DEST: returns non-zero when the download fails (the caller decides what that means).
download() {
    need curl
    curl -fsSL --retry 3 -o "$2" "$1"
}

sha256_of() {
    if have sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    elif have shasum; then
        shasum -a 256 "$1" | cut -d' ' -f1
    elif have openssl; then
        openssl dgst -sha256 "$1" | sed 's/^.*= //'
    else
        die "sha256sum, shasum or openssl is required to verify downloads"
    fi
}

# verify_checksum FILE SUMFILE: the first field of SUMFILE (a sha256sum line) must be FILE's digest.
verify_checksum() {
    expected="$(cut -d' ' -f1 "$2" | tr 'A-F' 'a-f' | head -n 1)"
    [ -n "$expected" ] || die "the checksum file $2 is empty"
    actual="$(sha256_of "$1")"
    [ "$expected" = "$actual" ] || die "checksum mismatch for $1: expected $expected, got $actual"
}

# fetch_verified URL DEST: download URL and URL.sha256 (required), verify, and leave DEST.
fetch_verified() {
    download "$1" "$2" || die "failed to download $1"
    download "$1.sha256" "$2.sha256" 2>/dev/null \
        || die "$1 must be published with its .sha256 file, next to it"
    verify_checksum "$2" "$2.sha256"
    rm -f "$2.sha256"
}

# is_package DIR: DIR is an extracted release package.
is_package() { [ -f "$1/PACKAGE_FORMAT" ] && [ -d "$1/bin" ] && [ -d "$1/wheels" ]; }

# package_in DIR: DIR when it is a package, else its only subdirectory that is one.
package_in() {
    if is_package "$1"; then
        printf '%s' "$1"
        return 0
    fi
    found=""
    count=0
    for entry in "$1"/*; do
        [ -d "$entry" ] || continue
        count=$((count + 1))
        found="$entry"
    done
    if [ "$count" -eq 1 ] && is_package "$found"; then
        printf '%s' "$found"
        return 0
    fi
    return 1
}

# own_package: the package this script is running from (<package>/sbin/<script>), when it is. A script
# piped from a URL has no file of its own, so it never is.
own_package() {
    [ -f "$0" ] || return 1
    dir="$(cd "$(dirname "$0")/.." 2>/dev/null && pwd)" || return 1
    is_package "$dir" || return 1
    printf '%s' "$dir"
}

# check_package_format DIR: this script supports the package's layout.
check_package_format() {
    format="$(cat "$1/PACKAGE_FORMAT")"
    [ "$format" = "$FLAME_PACKAGE_FORMAT" ] \
        || die "the package has format $format, this script supports format $FLAME_PACKAGE_FORMAT; use a newer one"
}

# confirm QUESTION: ask on the terminal and return 0 for yes, 1 for no, 2 when there is no terminal. The
# answer comes from the terminal, not stdin, which is the script itself when it is piped from curl.
# FLAME_TTY names another file to read the answer from (tests).
confirm() {
    tty="${FLAME_TTY:-/dev/tty}"
    (: < "$tty") 2>/dev/null || return 2
    printf '%s [y/N]: ' "$1" >&2
    read -r answer < "$tty" || answer=""
    case "$answer" in y | Y | yes | YES) return 0 ;; esac
    return 1
}

# pep440 TAG: the Python version of a release tag (v0.7.0 is 0.7.0, v0.7.0-rc3 is 0.7.0rc3).
pep440() {
    printf '%s' "$1" | sed -e 's/^v//' -e 's/-rc\.\{0,1\}\([0-9]\)/rc\1/'
}

# flmenv_template: the path of flmenv.sh.in, from the checkout or, in a hosted script, a copy written
# from the template embedded by inline.sh.
flmenv_template() {
    if [ -n "${FLAME_FLMENV_TEMPLATE:-}" ] && [ -f "$FLAME_FLMENV_TEMPLATE" ]; then
        printf '%s' "$FLAME_FLMENV_TEMPLATE"
    elif [ -f "${FLAME_LIB_DIR:-/nonexistent}/../flmenv.sh.in" ]; then
        printf '%s' "$FLAME_LIB_DIR/../flmenv.sh.in"
    else
        embedded="$(mktemp)"
        cat > "$embedded" <<'FLAME_FLMENV_TEMPLATE'
#@include flmenv.sh.in
FLAME_FLMENV_TEMPLATE
        printf '%s' "$embedded"
    fi
}

# render_flmenv TEMPLATE OUT PREFIX VERSIONS: write OUT from TEMPLATE with @PREFIX@ and
# @PYTHON_VERSIONS@ replaced; VERSIONS is a space-separated list, rendered as "3.11" "3.12".
render_flmenv() {
    quoted=""
    for version in $4; do
        quoted="$quoted\"$version\" "
    done
    quoted="${quoted% }"
    sed -e "s|@PREFIX@|$3|g" -e "s|@PYTHON_VERSIONS@|$quoted|g" "$1" > "$2"
    chmod 0755 "$2"
}

# python_version_of PYTHON: major.minor of the interpreter, empty when it does not run.
python_version_of() {
    "$1" -c 'import sys; print("%d.%d" % sys.version_info[:2])' 2>/dev/null || true
}

# newest_version LIST: the highest major.minor in a space-separated list.
newest_version() {
    # shellcheck disable=SC2086 # the list is space-separated on purpose
    printf '%s\n' $1 | sort -t. -k1,1n -k2,2n | tail -n 1
}

# load_dotenv FILE: export the FLAME_* settings of a .env file (KEY=VALUE lines, # comments, optional
# "export " and quotes) that the environment does not already set; the environment wins.
load_dotenv() {
    [ -f "$1" ] || return 0
    while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in ''|'#'*|*=*) ;; *) continue ;; esac
        case "$line" in ''|'#'*) continue ;; esac
        line="${line#export }"
        key="${line%%=*}"
        val="${line#*=}"
        case "$key" in FLAME_*) ;; *) continue ;; esac
        case "$key" in *[!A-Za-z0-9_]*) continue ;; esac
        case "$val" in
            \"*\") val="${val#\"}"; val="${val%\"}" ;;
            \'*\') val="${val#\'}"; val="${val%\'}" ;;
        esac
        eval "[ -z \"\${$key+x}\" ]" || continue
        export "$key=$val"
    done < "$1"
}

# check_settings NAME...: refuse a setting that cannot go into double-quoted YAML. It runs in the script's
# own shell, so die stops the script; setting runs in a subshell and cannot.
check_settings() {
    for name in "$@"; do
        eval "value=\"\${$name:-}\""
        case "$value" in
            *\"*|*"
"*) die "$name must not contain a double quote or a newline" ;;
        esac
    done
}

# setting NAME DEFAULT: the value of $NAME, else DEFAULT.
setting() {
    eval "value=\"\${$1:-}\""
    [ -n "$value" ] || value="$2"
    printf '%s' "$value"
}
