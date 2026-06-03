.PHONY: all clean cargo-setup busybox-la lua-la initramfs-la kernel-la kernel-rv

all: cargo-setup busybox-la lua-la initramfs-la kernel-la

cargo-setup:
	@if [ ! -d .cargo ] && [ -d cargo-config ]; then \
		cp -r cargo-config .cargo; \
		echo "cargo-config → .cargo"; \
	fi

busybox-la:
	sh scripts/build-busybox.sh la

lua-la:
	sh scripts/build-lua.sh la

initramfs-la: busybox-la lua-la
	sh scripts/build-initramfs.sh la

kernel-la: initramfs-la
	cargo build -p kernel --target loongarch64-unknown-none --features embedded-initramfs --release
	cp target/loongarch64-unknown-none/release/kernel kernel-la

kernel-rv:
	@echo "RISC-V 尚未实现，跳过 kernel-rv 构建"

clean:
	cargo clean
	rm -f kernel-la kernel-rv build/initramfs.cpio
