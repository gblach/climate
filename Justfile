targets := "aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu"
version := `cargo pkgid | cut -d'#' -f2`

release:
    for target in {{targets}}; do \
        cargo zigbuild --release --target $target; \
        mv target/$target/release/climate climate-{{version}}-$target; \
    done
