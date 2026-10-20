##################################################
# Variables
#

rust_edition := "2021"
open := if os() == "linux" { "xdg-open" } else if os() == "macos" { "open" } else { "start \"\" /max" }
app_name := "bella-presenter"
crate_name := "bella_presenter"
args := ""
project_directory := justfile_directory()
release := `git describe --tags --always 2>/dev/null || echo v0.1.0`
version := "0.3.0"
url := "https://github.com/tschinz/bella-presenter"
file := "examples/04-aprog-ptr-en.pdf"
notes_file := "examples/04-aprog-ptr-notes-en.pdf"

# Prebuilt PDFium library asset for this platform (from bblanchon/pdfium-binaries)
pdfium_asset := if os() == "macos" {
    if arch() == "aarch64" { "pdfium-mac-arm64.tgz" } else { "pdfium-mac-x64.tgz" }
} else if os() == "linux" {
    if arch() == "aarch64" { "pdfium-linux-arm64.tgz" } else { "pdfium-linux-x64.tgz" }
} else {
    "pdfium-win-x64.tgz"
}

# For windows shell to be supported (suppose code is multi-platforms ready)
set shell := ["bash", "-uc"]
set windows-shell := ["cmd.exe", "/c"]

##################################################
# Default
#

# List all available commands
default:
    @just --list

##################################################
# Info & Dependencies
#

# Print environment info (OS, arch, toolchains, PDFium asset)
info:
    #!/usr/bin/env bash
    set +e
    echo "OS          : {{ os() }} ({{ arch() }})"
    echo "Project     : {{ project_directory }}"
    echo "App         : {{ app_name }}"
    echo "Version     : {{ version }}"
    echo "Open        : {{ open }}"
    echo "PDFium asset: {{ pdfium_asset }}"
    echo ""
    echo "--- Rust toolchain ---"
    rustup show 2>/dev/null || echo "rustup not found"

# Check that required tools are available (build tools fatal; the rest warn)
check-deps:
    #!/usr/bin/env bash
    set +e
    errors=0
    warnings=0
    ok()   { printf "  ✓ %-13s %s\n" "$1" "$2"; }
    warn() { printf "  ⚠ %-13s %s\n" "$1" "$2"; warnings=$((warnings+1)); }
    fail() { printf "  ✗ %-13s %s\n" "$1" "$2"; errors=$((errors+1)); }

    echo "--- Build tools ---"
    if v=$(cargo --version 2>/dev/null);        then ok   "cargo"   "$v"; else fail "cargo"   "not found - install Rust: https://rustup.rs"; fi
    if v=$(rustfmt --version 2>/dev/null);      then ok   "rustfmt" "$v"; else fail "rustfmt" "not found - run: rustup component add rustfmt"; fi
    if v=$(cargo clippy --version 2>/dev/null); then ok   "clippy"  "$v"; else fail "clippy"  "not found - run: rustup component add clippy"; fi

    echo ""
    echo "--- Optional tooling ---"
    if v=$(cargo bundle --version 2>/dev/null); then ok   "cargo-bundle" "$v"; else warn "cargo-bundle" "not found - run: cargo install cargo-bundle (for 'just bundle')"; fi
    if v=$(git cliff --version 2>/dev/null);    then ok   "git-cliff"    "$v"; else warn "git-cliff"    "not found - run: cargo install git-cliff (for 'just changelog')"; fi
    if v=$(cargo sbom --version 2>/dev/null);   then ok   "cargo-sbom"   "$v"; else warn "cargo-sbom"   "not found - run: cargo install cargo-sbom (for 'just sbom')"; fi

    echo ""
    echo "--- PDFium runtime library ---"
    if ls third_party/pdfium/lib/libpdfium.* >/dev/null 2>&1 || ls third_party/pdfium/lib/pdfium.dll >/dev/null 2>&1; then
        ok   "pdfium" "present in third_party/pdfium/lib"
    else
        warn "pdfium" "not found - run 'just setup-pdfium'"
    fi

    echo ""
    if [[ $errors -gt 0 ]]; then
        echo "✗ $errors error(s) found - fix the above before building."
        exit 1
    elif [[ $warnings -gt 0 ]]; then
        echo "✓ Build tools present ($warnings optional warning(s) - see above)."
    else
        echo "✓ All dependencies satisfied."
    fi

# One-shot developer setup: toolchain + PDFium library
setup: check-deps setup-pdfium
    @echo "✓ Setup complete - run 'just run' to start"

# Download the prebuilt PDFium library for this platform
setup-pdfium:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p third_party/pdfium
    cd third_party/pdfium
    echo "Downloading {{ pdfium_asset }} ..."
    curl --proto '=https' --tlsv1.2 -sSL -o pdfium.tgz \
        "https://github.com/bblanchon/pdfium-binaries/releases/latest/download/{{ pdfium_asset }}"
    tar xzf pdfium.tgz
    rm pdfium.tgz
    echo "✓ PDFium installed to third_party/pdfium/lib"

# Ensure the PDFium library is present (download it if missing)
ensure-pdfium:
    #!/usr/bin/env bash
    if ls third_party/pdfium/lib/libpdfium.* >/dev/null 2>&1 \
        || ls third_party/pdfium/lib/pdfium.dll >/dev/null 2>&1; then
        echo "✓ PDFium already present"
    else
        just setup-pdfium
    fi

# Install toolchain + cargo tooling + PDFium
install:
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
    cargo install cargo-sbom
    cargo install cargo-bundle
    cargo install cargo-about --features cli
    just setup-pdfium

# install the release version (default is the latest)
install-release release=release:
    cargo install --git {{ url }} --tag {{ release }}

# install the nightly release
install-nightly:
    cargo install --git {{ url }}

##################################################
# Build & Run
#

# Build and copy the release version of the program
build:
    cargo build --release
    mkdir -p bin && cp target/release/{{ app_name }} bin/

# Run cargo check (fast compile check, no codegen)
check:
    cargo check

# Run the presenter (debug) on a PDF
run file=file args=args:
    cargo run -- {{ file }} {{ args }}

# Run the presenter with no file — shows the start screen with recent files
run-empty args=args:
    cargo run -- {{ args }}

# Run the presenter (release) on a PDF
run-release file=file args=args:
    cargo run --release -- {{ file }} {{ args }}

# Open the example deck that has speaker notes
run-notes args=args:
    cargo run -- {{ notes_file }} {{ args }}

##################################################
# Bundle (self-contained app packages, ship PDFium inside)
#
# Bundles must be built on their target OS (cargo-bundle does not cross-build).

# Bundle the app for the current OS (release)
bundle: ensure-pdfium
    cargo bundle --release

# macOS .app bundle (ad-hoc code-signed so macOS won't report it as "damaged")
bundle-mac: ensure-pdfium
    #!/usr/bin/env bash
    set -euo pipefail
    rm -rf target/release/bundle/osx
    cargo bundle --release --format osx
    # cargo-bundle adds files after the linker's signature, which invalidates it and
    # makes macOS refuse the app. Re-sign the whole bundle ad-hoc (no certificate).
    app=$(echo target/release/bundle/osx/*.app)
    codesign --force --deep --sign - "$app"
    codesign --verify --strict "$app" && echo "✓ Ad-hoc signed: $app"

# Linux .deb package
bundle-deb: ensure-pdfium
    cargo bundle --release --format deb

# Linux AppImage
bundle-appimage: ensure-pdfium
    cargo bundle --release --format appimage

# Windows .msi installer (run on Windows, needs WiX)
bundle-msi: ensure-pdfium
    cargo bundle --release --format msi

##################################################
# Test & Lint
#

# Run all tests
test:
    cargo test

# Run clippy with strict warnings
clippy:
    cargo clippy --all-targets --all-features -- -D warnings

# Format code (rustfmt.toml) and lint it with clippy
lint:
    cargo fmt --all
    cargo clippy --all-targets -- -D warnings

# Regenerate the third-party license list shown on the About page (needs cargo-about).
# Install the tool with: cargo install cargo-about --features cli
thirdparty:
    cargo about generate -c about/about.toml about/about.hbs -o assets/thirdparty.md
    @echo "✓ Wrote assets/thirdparty.md"

##################################################
# Documentation
#

# Generate and open rustdoc documentation
doc:
    @echo "Generating rustdoc documentation..."
    cargo doc --no-deps --document-private-items
    @echo "✓ Documentation generated"
    @echo "Opening documentation in browser..."
    {{ open }} target/doc/{{ crate_name }}/index.html

# Generate rustdoc documentation without opening
doc-build:
    @echo "Generating rustdoc documentation..."
    cargo doc --no-deps --document-private-items
    @echo "✓ Documentation generated at target/doc/{{ crate_name }}/index.html"

##################################################
# Release
#

# Prepend the unreleased changes to CHANGELOG.md for the given version
changelog version=version:
    git cliff --unreleased --tag {{ version }} --prepend CHANGELOG.md

# Generate SBOM for Dependency Track
sbom:
    cargo sbom --output-format cyclone_dx_json_1_6 >> target/sbom-cyclone_dx_1_6.json

# Upload SBOM to Dependency Track (requires DT_API_KEY, DT_PROJECT_UUID, DT_BASE_URL env vars)
sbom-upload: sbom
    #!/usr/bin/env bash
    set -euo pipefail
    echo "Uploading SBOM to Dependency Track..."
    # Load .env file if it exists
    if [[ -f .env ]]; then
        echo "Loading configuration from .env file..."
        export $(grep -v '^#' .env | grep -v '^$' | xargs)
    fi
    if [[ -z "${DT_API_KEY:-}" ]] || [[ -z "${DT_PROJECT_UUID:-}" ]] || [[ -z "${DT_BASE_URL:-}" ]]; then
        echo "Error: Required environment variables not set:"
        echo "  DT_API_KEY - Your Dependency Track API key"
        echo "  DT_PROJECT_UUID - Your project UUID"
        echo "  DT_BASE_URL - Your Dependency Track base URL"
        exit 1
    fi
    just sbom
    curl -X POST "${DT_BASE_URL}/api/v1/bom" \
        -H "X-Api-Key: ${DT_API_KEY}" \
        -H "Content-Type: multipart/form-data" \
        -F "project=${DT_PROJECT_UUID}" \
        -F "bom=@target/sbom-cyclone_dx_1_6.json"
    echo "✓ SBOM uploaded successfully to Dependency Track"

# Trivy comprehensive security scan
trivy:
    trivy fs --scanners vuln,secret,misconfig --format table .

##################################################
# Clean
#

# Clean build artifacts
clean:
    cargo clean
    @rm -rf {{ project_directory / "bin" }}
    @echo "Clean complete."

##################################################
# Release Readiness
#

# Check steps for publishing is_lib ["true"|"false"]
publish-check is_lib="false":
    #!/usr/bin/env bash
    echo "Run all tests"
    cargo test
    echo "Run clippy"
    cargo clippy
    echo "Format code"
    cargo fmt --all
    echo "Build documentation"
    cargo doc --no-deps
    echo "Test documentation examples"
    if [ "{{ is_lib }}" = "true" ]; then
        cargo test --doc
    fi
    echo "Run security audit"
    cargo audit
    echo "Test Publishing"
    cargo publish --dry-run

# Show help for the compiled binary
help:
    cargo run -- --help
