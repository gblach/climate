name := `cargo pkgid | sed -E 's/.*#([^@]+)@.*/\1/;t;s|.*/([^/#]+)#.*|\1|'`
version := `cargo pkgid | sed -E 's/.*#//; s/.*@//'`
targets := "aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu"
seccomp_version := "2.6.1"
vendor := justfile_directory() / "target" / "vendor"

# Build the seccomp library once for each release target. It is a C library, so the linker needs a
# copy built for the target's architecture, and a static one at that, so the released binaries
# carry it instead of looking for a matching libseccomp.so wherever they run. Needs curl, make and
# gperf, which libseccomp's build uses to generate its syscall tables.
[private]
seccomp:
    #!/usr/bin/env bash
    set -euo pipefail
    source="{{ vendor }}/libseccomp-{{ seccomp_version }}"
    release="https://github.com/seccomp/libseccomp/releases/download/v{{ seccomp_version }}"
    if [ ! -d "$source" ]; then
        mkdir -p "{{ vendor }}"
        archive="libseccomp-{{ seccomp_version }}.tar.gz"
        curl -sSL -o "{{ vendor }}/$archive" "$release/$archive"
        curl -sSL -o "{{ vendor }}/$archive.SHA256SUM" "$release/$archive.SHA256SUM"
        (cd "{{ vendor }}" && sha256sum -c "$archive.SHA256SUM")
        tar xf "{{ vendor }}/$archive" -C "{{ vendor }}"
    fi
    for target in {{ targets }}; do
        prefix="{{ vendor }}/$target"
        if [ -f "$prefix/lib/libseccomp.a" ]; then
            continue
        fi
        # zig spells a target the same way cargo does, without the "unknown" in the middle.
        triple="${target/-unknown/}"
        build="{{ vendor }}/build-$target"
        rm -rf "$build" && mkdir -p "$build"
        (cd "$build" && "$source/configure" --host="$triple" --prefix="$prefix" \
            --enable-static --disable-shared \
            CC="zig cc -target $triple" AR="zig ar" RANLIB="zig ranlib" >/dev/null)
        # Only the library itself is built; the command line tools that come with it are not
        # needed and would have to be linked for the target as well.
        make -C "$build/src" -j"$(nproc)" >/dev/null
        make -C "$build/src" install >/dev/null
        # The description pkg-config reads is prepared at the top of the build tree, and installing
        # it is part of a step we skip above, so it is put in place here.
        mkdir -p "$prefix/lib/pkgconfig"
        cp "$build/libseccomp.pc" "$prefix/lib/pkgconfig/"
    done

release: seccomp
    #!/usr/bin/env bash
    set -euo pipefail
    for target in {{ targets }}; do
        # Two crates find the seccomp library in two different ways, and both have to be pointed at
        # the copy built above: libseccomp-sys reads these two variables, while libseccomp asks
        # pkg-config, which links a shared library unless told otherwise. Miss either one and the
        # build picks up this machine's own libseccomp.so and fails on the other architecture.
        LIBSECCOMP_LIB_PATH="{{ vendor }}/$target/lib" \
        LIBSECCOMP_LINK_TYPE=static \
        PKG_CONFIG_ALL_STATIC=1 \
            cargo zigbuild --release --target "$target"
        mv "target/$target/release/{{ name }}" "{{ name }}-{{ version }}-$target"
    done
