.PHONY: all clean cargo-setup kernel-la kernel-rv

all: cargo-setup kernel-la

cargo-setup:
	@if [ ! -d .cargo ] && [ -d cargo-config ]; then \
		cp -r cargo-config .cargo; \
		echo "cargo-config -> .cargo"; \
	fi

kernel-la: cargo-setup
	cargo build -p kernel --target loongarch64-unknown-none --features embedded-initramfs --release
	cp target/loongarch64-unknown-none/release/kernel kernel-la

kernel-rv:
	@echo "RISC-V 尚未实现，跳过 kernel-rv 构建"

clean:
	cargo clean
	rm -f kernel-la kernel-rv build/initramfs.cpio
