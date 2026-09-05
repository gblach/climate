name := `cargo pkgid | sed -E 's/.*#([^@]+)@.*/\1/;t;s|.*/([^/#]+)#.*|\1|'`
version := `cargo pkgid | sed -E 's/.*#//; s/.*@//'`
targets := "aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu"

release:
    for target in {{targets}}; do \
        cargo zigbuild --release --target $target; \
        mv target/$target/release/{{name}} {{name}}-{{version}}-$target; \
    done
