.PHONY: all clean cargo-setup kernel-la kernel-rv rootfs-la rootfs-rv

all: cargo-setup kernel-la kernel-rv

JOBS ?= $(shell nproc 2>/dev/null || echo 4)
BUILD_DIR := build

LA_TARGET := loongarch64-unknown-none
LA_ROOTFS := userland/rootfs-la
LA_INITRAMFS := $(BUILD_DIR)/initramfs-la.cpio
LA_CROSS_COMPILE := loongarch64-linux-gnu-
LA_KERNEL := kernel-la

RV_TARGET := riscv64gc-unknown-none-elf
RV_ROOTFS := userland/rootfs-rv
RV_INITRAMFS := $(BUILD_DIR)/initramfs-rv.cpio
RV_CROSS_COMPILE := riscv64-linux-musl-
RV_KERNEL := kernel-rv

BUSYBOX_SRC := third/busybox-1.36.1

cargo-setup:
	@if [ ! -d .cargo ] && [ -d cargo-config ]; then \
		cp -r cargo-config .cargo; \
		echo "cargo-config → .cargo"; \
	fi

kernel-la: cargo-setup rootfs-la
	INITRAMFS_ROOT=$(LA_ROOTFS) INITRAMFS_CPIO=$(LA_INITRAMFS) \
		cargo build -p kernel --target $(LA_TARGET) --features embedded-initramfs --release
	cp target/$(LA_TARGET)/release/kernel $(LA_KERNEL)

kernel-rv: cargo-setup rootfs-rv
	INITRAMFS_ROOT=$(RV_ROOTFS) INITRAMFS_CPIO=$(RV_INITRAMFS) \
		cargo build -p kernel --target $(RV_TARGET) --features embedded-initramfs --release
	cp target/$(RV_TARGET)/release/kernel $(RV_KERNEL)

rootfs-la: $(LA_ROOTFS)/bin/busybox

rootfs-rv: $(RV_ROOTFS)/bin/busybox

$(LA_ROOTFS)/bin/busybox:
	$(MAKE) rootfs-busybox ROOTFS_DIR=$(LA_ROOTFS) CROSS_COMPILE=$(LA_CROSS_COMPILE)

$(RV_ROOTFS)/bin/busybox:
	$(MAKE) rootfs-busybox ROOTFS_DIR=$(RV_ROOTFS) CROSS_COMPILE=$(RV_CROSS_COMPILE)

.PHONY: rootfs-busybox
rootfs-busybox:
	$(MAKE) -C $(BUSYBOX_SRC) CROSS_COMPILE=$(CROSS_COMPILE) defconfig
	sed -i 's/.*CONFIG_STATIC.*/CONFIG_STATIC=y/' $(BUSYBOX_SRC)/.config
	sed -i 's/.*CONFIG_PIE.*/CONFIG_PIE=y/' $(BUSYBOX_SRC)/.config
	sed -i 's/^CONFIG_TC=.*/# CONFIG_TC is not set/' $(BUSYBOX_SRC)/.config
	yes '' | $(MAKE) -C $(BUSYBOX_SRC) CROSS_COMPILE=$(CROSS_COMPILE) oldconfig
	$(MAKE) -C $(BUSYBOX_SRC) CROSS_COMPILE=$(CROSS_COMPILE) -j$(JOBS)
	mkdir -p $(ROOTFS_DIR)
	$(MAKE) -C $(BUSYBOX_SRC) CROSS_COMPILE=$(CROSS_COMPILE) CONFIG_PREFIX=$(abspath $(ROOTFS_DIR)) install
	-$(CROSS_COMPILE)strip $(ROOTFS_DIR)/bin/busybox
	$(MAKE) -C $(BUSYBOX_SRC) distclean

clean:
	cargo clean
	rm -f $(LA_KERNEL) $(RV_KERNEL) build/initramfs.cpio $(LA_INITRAMFS) $(RV_INITRAMFS)
