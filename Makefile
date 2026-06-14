.PHONY: all clean cargo-setup kernel-la kernel-rv

all: cargo-setup kernel-la kernel-rv

cargo-setup:
	@if [ ! -d .cargo ] && [ -d cargo-config ]; then \
		cp -r cargo-config .cargo; \
		echo "cargo-config → .cargo"; \
	fi

kernel-la:
	cargo build -p kernel --target loongarch64-unknown-none --features embedded-initramfs --release
	cp target/loongarch64-unknown-none/release/kernel kernel-la

kernel-rv:
	cargo build -p kernel --target riscv64gc-unknown-none-elf --features embedded-initramfs --release
	cp target/riscv64gc-unknown-none-elf/release/kernel kernel-rv

clean:
	cargo clean
	rm -f kernel-la kernel-rv build/initramfs.cpio build/initramfs-la.cpio build/initramfs-rv.cpio
