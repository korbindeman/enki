# Run the enki CLI (passes through any args)
run *ARGS:
    cargo run --bin enki -- {{ARGS}}

# Run the TUI chat example
chat:
    cargo run -p enki-tui --example chat

# Build release and copy to ~/dev/_scripts
install:
    cargo build --release --bin enki
    cp target/release/enki ~/dev/_scripts/enki
