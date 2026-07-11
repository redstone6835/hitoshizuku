.PHONY: all clean cargo-setup kernel-la kernel-rv rootfs-la rootfs-rv rootfs-ltp-scenarios-la rootfs-ltp-scenarios-rv elm-smoke-la elm-smoke-rv elmctl-la elmctl-rv elmapi rootfs-elm-smoke-la rootfs-elm-smoke-rv rootfs-elmctl-la rootfs-elmctl-rv

all: cargo-setup kernel-la kernel-rv

JOBS ?= $(shell nproc 2>/dev/null || echo 4)
BUILD_DIR := build
CARGO_TARGET_DIR ?= target
FEATURES ?=
KERNEL_FEATURES = embedded-initramfs $(FEATURES)
CARGO_FEATURES = $(subst $(space),$(comma),$(strip $(KERNEL_FEATURES)))
empty :=
space := $(empty) $(empty)
comma := ,

LA_TARGET := loongarch64-unknown-none
LA_ROOTFS := userland/rootfs-la
LA_INITRAMFS := $(BUILD_DIR)/initramfs-la.cpio
LA_CROSS_COMPILE := loongarch64-linux-gnu-
LA_KERNEL := kernel-la
LA_ELM_SMOKE := $(BUILD_DIR)/elm-smoke-la/elmctl-smoke
LA_ELMCTL := $(BUILD_DIR)/elmctl-la/elmctl

RV_TARGET := riscv64gc-unknown-none-elf
RV_ROOTFS := userland/rootfs-rv
RV_INITRAMFS := $(BUILD_DIR)/initramfs-rv.cpio
RV_CROSS_COMPILE := riscv64-linux-musl-
RV_KERNEL := kernel-rv
RV_ELM_SMOKE := $(BUILD_DIR)/elm-smoke-rv/elmctl-smoke
RV_ELMCTL := $(BUILD_DIR)/elmctl-rv/elmctl

BUSYBOX_SRC := third/busybox-1.36.1
BUSYBOX_ARCHIVE := third/busybox-1.36.1.tar.gz
ENSURE_BUSYBOX := scripts/ensure-busybox.sh
LTP_SCENARIO_SRC := userland/ltp-scenarios
LTP_TESTCODE_SRC := userland/ltp_testcode.sh
ELM_SMOKE_SRC := userland/elmctl-smoke/elmctl_smoke.c
ELM_FINGERPRINT_GEN := scripts/gen-elm-fingerprint-header.sh
ELM_FINGERPRINT_INPUTS := $(wildcard libs/elm/src/*.rs libs/elm/src/mgr/*.rs tools/elm-tools/src/*.rs) \
	libs/elm/Cargo.toml tools/elm-tools/Cargo.toml
LA_ELM_FINGERPRINT := $(BUILD_DIR)/elm-smoke-la/elm_fingerprint.h
RV_ELM_FINGERPRINT := $(BUILD_DIR)/elm-smoke-rv/elm_fingerprint.h
ELMCTL_SRC := userland/elmctl/elmctl.c userland/elmctl/elmctl_client.c
ELMCTL_HEADERS := userland/elmctl/include/elmctl_abi.h userland/elmctl/include/elmctl_client.h
ELM_TOOLS_MANIFEST := tools/elm-tools/Cargo.toml
ELMAPI_OUTPUT := $(BUILD_DIR)/elmapi/v1/elmmgr
HOST_TARGET := x86_64-unknown-linux-gnu

cargo-setup:
	@if [ ! -d .cargo ] && [ -d cargo-config ]; then \
		cp -r cargo-config .cargo; \
		echo "cargo-config → .cargo"; \
	fi

elmapi:
	cargo run --manifest-path $(ELM_TOOLS_MANIFEST) --target $(HOST_TARGET) -- \
		generate-elmmgr $(ELMAPI_OUTPUT)

kernel-la: cargo-setup rootfs-la
	INITRAMFS_ROOT=$(LA_ROOTFS) INITRAMFS_CPIO=$(LA_INITRAMFS) \
		cargo build -p kernel --target $(LA_TARGET) --features "$(CARGO_FEATURES)" --release
	cp $(CARGO_TARGET_DIR)/$(LA_TARGET)/release/kernel $(LA_KERNEL)

kernel-rv: cargo-setup rootfs-rv
	INITRAMFS_ROOT=$(RV_ROOTFS) INITRAMFS_CPIO=$(RV_INITRAMFS) \
		cargo build -p kernel --target $(RV_TARGET) --features "$(CARGO_FEATURES)" --release
	cp $(CARGO_TARGET_DIR)/$(RV_TARGET)/release/kernel $(RV_KERNEL)

rootfs-la: $(LA_ROOTFS)/bin/busybox rootfs-ltp-scenarios-la rootfs-elm-smoke-la rootfs-elmctl-la

rootfs-rv: $(RV_ROOTFS)/bin/busybox rootfs-ltp-scenarios-rv rootfs-elm-smoke-rv rootfs-elmctl-rv

rootfs-ltp-scenarios-la:
	mkdir -p $(LA_ROOTFS)/etc/ltp-scenarios
	rm -f $(LA_ROOTFS)/etc/ltp-scenarios/*
	cp $(LTP_SCENARIO_SRC)/* $(LA_ROOTFS)/etc/ltp-scenarios/
	cp $(LTP_TESTCODE_SRC) $(LA_ROOTFS)/etc/ltp_testcode.sh
	chmod +x $(LA_ROOTFS)/etc/ltp_testcode.sh

rootfs-ltp-scenarios-rv:
	mkdir -p $(RV_ROOTFS)/etc/ltp-scenarios
	rm -f $(RV_ROOTFS)/etc/ltp-scenarios/*
	cp $(LTP_SCENARIO_SRC)/* $(RV_ROOTFS)/etc/ltp-scenarios/
	cp $(LTP_TESTCODE_SRC) $(RV_ROOTFS)/etc/ltp_testcode.sh
	chmod +x $(RV_ROOTFS)/etc/ltp_testcode.sh

$(LA_ROOTFS)/bin/busybox:
	$(MAKE) rootfs-busybox ROOTFS_DIR=$(LA_ROOTFS) CROSS_COMPILE=$(LA_CROSS_COMPILE)

$(RV_ROOTFS)/bin/busybox:
	$(MAKE) rootfs-busybox ROOTFS_DIR=$(RV_ROOTFS) CROSS_COMPILE=$(RV_CROSS_COMPILE)

elm-smoke-la: $(LA_ELM_SMOKE)

elm-smoke-rv: $(RV_ELM_SMOKE)

elmctl-la: $(LA_ELMCTL)

elmctl-rv: $(RV_ELMCTL)

rootfs-elm-smoke-la: $(LA_ROOTFS)/bin/elmctl-smoke

rootfs-elm-smoke-rv: $(RV_ROOTFS)/bin/elmctl-smoke

rootfs-elmctl-la: $(LA_ROOTFS)/bin/elmctl

rootfs-elmctl-rv: $(RV_ROOTFS)/bin/elmctl

$(LA_ELM_FINGERPRINT): $(ELM_FINGERPRINT_GEN) $(ELM_FINGERPRINT_INPUTS)
	$(ELM_FINGERPRINT_GEN) $(LA_TARGET) $@

$(RV_ELM_FINGERPRINT): $(ELM_FINGERPRINT_GEN) $(ELM_FINGERPRINT_INPUTS)
	$(ELM_FINGERPRINT_GEN) $(RV_TARGET) $@

$(LA_ELM_SMOKE): $(ELM_SMOKE_SRC) $(LA_ELM_FINGERPRINT)
	mkdir -p $(dir $@)
	$(LA_CROSS_COMPILE)gcc -std=c11 -static -Os -Wall -Wextra -I$(dir $@) $< -o $@
	-$(LA_CROSS_COMPILE)strip $@

$(RV_ELM_SMOKE): $(ELM_SMOKE_SRC) $(RV_ELM_FINGERPRINT)
	mkdir -p $(dir $@)
	$(RV_CROSS_COMPILE)gcc -std=c11 -static -Os -Wall -Wextra -I$(dir $@) $< -o $@
	-$(RV_CROSS_COMPILE)strip $@

$(LA_ELMCTL): $(ELMCTL_SRC) $(ELMCTL_HEADERS)
	mkdir -p $(dir $@)
	$(LA_CROSS_COMPILE)gcc -std=c11 -static -Os -Wall -Wextra -Iuserland/elmctl/include $(ELMCTL_SRC) -o $@
	-$(LA_CROSS_COMPILE)strip $@

$(RV_ELMCTL): $(ELMCTL_SRC) $(ELMCTL_HEADERS)
	mkdir -p $(dir $@)
	$(RV_CROSS_COMPILE)gcc -std=c11 -static -Os -Wall -Wextra -Iuserland/elmctl/include $(ELMCTL_SRC) -o $@
	-$(RV_CROSS_COMPILE)strip $@

$(LA_ROOTFS)/bin/elmctl-smoke: $(LA_ELM_SMOKE)
	mkdir -p $(LA_ROOTFS)/bin
	install -m 0755 $< $@

$(RV_ROOTFS)/bin/elmctl-smoke: $(RV_ELM_SMOKE)
	mkdir -p $(RV_ROOTFS)/bin
	install -m 0755 $< $@

$(LA_ROOTFS)/bin/elmctl: $(LA_ELMCTL)
	mkdir -p $(LA_ROOTFS)/bin
	install -m 0755 $< $@

$(RV_ROOTFS)/bin/elmctl: $(RV_ELMCTL)
	mkdir -p $(RV_ROOTFS)/bin
	install -m 0755 $< $@

.PHONY: rootfs-busybox
rootfs-busybox: $(ENSURE_BUSYBOX) $(BUSYBOX_ARCHIVE)
	$(ENSURE_BUSYBOX)
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
	rm -rf $(BUILD_DIR)/elm-smoke-la $(BUILD_DIR)/elm-smoke-rv $(BUILD_DIR)/elmctl-la $(BUILD_DIR)/elmctl-rv
