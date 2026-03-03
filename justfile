# Run the enki CLI (passes through any args)
run *ARGS:
    cargo run --bin enki -- {{ARGS}}

# Run the TUI chat example
chat:
    cargo run -p enki-tui --example chat

# Build and install release binary to ~/.cargo/bin
install:
    cargo install --path crates/cli
