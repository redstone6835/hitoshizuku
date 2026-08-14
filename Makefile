.DEFAULT_GOAL := kernel

.PHONY: default kernel modules modules_install config oldconfig defconfig busybox \
	kernel-la kernel-rv all clean cargo-setup \
	native-hello-la native-hello-rv native-rust-hello-la native-rust-hello-rv \
	native-parent-la native-parent-rv native-rust-parent-la native-rust-parent-rv \
	native-component-la native-component-rv native-repository-la native-repository-rv \
	native-ring-io-la native-ring-io-rv native-socket-ring-la native-socket-ring-rv \
	native-device-ring-la native-device-ring-rv \
	kcsan-la kcsan-rv syscall-bench-rv mm-bench-rv \
	instruction-weight-rv \
	_kernel-loongarch64 _kernel-riscv64 _modules-loongarch64 _modules-riscv64 \
	_busybox-loongarch64 _busybox-riscv64 \
	_compat-kernel-loongarch64 _compat-kernel-riscv64

default: kernel

JOBS ?= $(shell nproc 2>/dev/null || echo 4)
BUILD_DIR := build
CARGO_TARGET_DIR ?= target
FEATURES ?=
NATIVE_EXAMPLES ?=

ifneq ($(filter soyo-tests,$(FEATURES)),)
ifeq ($(strip $(NATIVE_EXAMPLES)),)
NATIVE_EXAMPLES := component ring-io socket-ring device-ring
endif
endif

NATIVE_EXAMPLE_COMMANDS := $(addprefix /bin/soyo-,$(NATIVE_EXAMPLES))
TEST_MODE ?= default
TEST_WORKLOAD ?=
SYSCALL_BENCH_ITERATIONS ?= 1000000
SYSCALL_BENCH_REPEATS ?= 5
SYSCALL_BENCH_CASE ?= all
SYSCALL_BENCH_WARMUP ?= 100000
MM_BENCH_CASE ?= anon-write
MM_BENCH_PAGES ?= 1
MM_BENCH_THREADS ?= 1
MM_BENCH_REPEATS ?= 1
RISCV_WEIGHT_BASE_BLOCKS ?= 256
RISCV_WEIGHT_ROUNDS ?= 9
RISCV_WEIGHT_CASE ?= all
RISCV_WEIGHT_RUN_ID ?= default
PROFILE_MODE ?= sample
PROFILE_PRESET ?= all
PROFILE_SAMPLE_HZ ?= 250
PROFILE_WORKLOAD ?=
PROFILE_PHASE_RULES ?=
CONFIG_FILE ?= .config
INITRAMFS ?=
INSTALL_MOD_PATH ?=
KERNEL_MAP ?=
KERNEL_PUBLISH_OUTPUT ?=
KCSAN_BUILD ?= 0

KCSAN_RUSTC_WRAPPER := $(abspath scripts/kcsan-rustc-wrapper.sh)
KCSAN_BUILD_DIR := $(abspath build/kcsan)
KCSAN_TARGET_DIR := $(abspath target/kcsan)
KCSAN_WRAPPER_ENV := $(if $(and $(filter 1,$(KCSAN_BUILD)),$(filter kcsan,$(FEATURES))),RUSTC_WRAPPER=$(KCSAN_RUSTC_WRAPPER),)
KERNEL_INTERFACE_TARGET_DIR := $(if $(filter 1,$(KCSAN_BUILD)),$(abspath $(BUILD_DIR)/cargo-kernel-interface-target),$(CARGO_TARGET_DIR))
KERNEL_INTERFACE_BUILD_ENV := $(if $(filter 1,$(KCSAN_BUILD)),env -u RUSTC_WRAPPER -u RUSTC_WORKSPACE_WRAPPER CARGO_TARGET_DIR=$(KERNEL_INTERFACE_TARGET_DIR),)

ifneq ($(filter kcsan,$(FEATURES)),)
ifeq ($(KCSAN_BUILD),0)
ifeq ($(strip $(MAKECMDGOALS)),)
$(error KCSAN 调试构建必须使用 make kcsan-la 或 make kcsan-rv)
endif
ifneq ($(strip $(filter-out kcsan-la kcsan-rv clean,$(MAKECMDGOALS))),)
$(error KCSAN 调试构建必须使用独立的 kcsan-la/kcsan-rv 目标)
endif
endif
endif

UNKNOWN_NATIVE_EXAMPLES := $(filter-out hello rust-hello parent rust-parent component repository ring-io socket-ring device-ring,$(NATIVE_EXAMPLES))
ifneq ($(strip $(UNKNOWN_NATIVE_EXAMPLES)),)
$(error NATIVE_EXAMPLES 包含未知示例: $(UNKNOWN_NATIVE_EXAMPLES))
endif

ifneq ($(filter component,$(NATIVE_EXAMPLES)),)
define install_native_component
	$(MAKE) -C native ARCH=$(1) component
	install -m 0755 $(BUILD_DIR)/$(1)/native/component-host.soyo $(2)/bin/soyo-component
endef
else
define install_native_component
endef
endif

ifneq ($(filter repository,$(NATIVE_EXAMPLES)),)
define install_native_repository
	$(MAKE) -C native ARCH=$(1) repository-client
	install -m 0755 $(BUILD_DIR)/$(1)/native/repository-client.soyo $(2)/bin/soyo-repository
endef
else
define install_native_repository
endef
endif

ifneq ($(filter hello,$(NATIVE_EXAMPLES)),)
define install_native_hello
	$(MAKE) -C native ARCH=$(1) hello
	install -m 0755 $(BUILD_DIR)/$(1)/native/hello.soyo $(2)/bin/soyo-hello
endef
else
define install_native_hello
endef
endif

ifneq ($(filter rust-hello,$(NATIVE_EXAMPLES)),)
define install_native_rust_hello
	$(MAKE) -C native ARCH=$(1) rust-hello
	install -m 0755 $(BUILD_DIR)/$(1)/native/rust-hello.soyo $(2)/bin/soyo-rust-hello
endef
else
define install_native_rust_hello
endef
endif

ifneq ($(filter parent,$(NATIVE_EXAMPLES)),)
define install_native_parent
	$(MAKE) -C native ARCH=$(1) native-parent
	install -m 0755 $(BUILD_DIR)/$(1)/native/parent.soyo $(2)/bin/soyo-parent
endef
else
define install_native_parent
endef
endif

ifneq ($(filter rust-parent,$(NATIVE_EXAMPLES)),)
define install_native_rust_parent
	$(MAKE) -C native ARCH=$(1) native-rust-parent
	install -m 0755 $(BUILD_DIR)/$(1)/native/rust-parent.soyo $(2)/bin/soyo-rust-parent
endef
else
define install_native_rust_parent
endef
endif

ifneq ($(filter ring-io,$(NATIVE_EXAMPLES)),)
define install_native_ring_io
	$(MAKE) -C native ARCH=$(1) ring-io
	install -m 0755 $(BUILD_DIR)/$(1)/native/ring-io.soyo $(2)/bin/soyo-ring-io
endef
else
define install_native_ring_io
endef
endif

ifneq ($(filter socket-ring,$(NATIVE_EXAMPLES)),)
define install_native_socket_ring
	$(MAKE) -C native ARCH=$(1) socket-ring
	install -m 0755 $(BUILD_DIR)/$(1)/native/socket-ring.soyo $(2)/bin/soyo-socket-ring
endef
else
define install_native_socket_ring
endef
endif

ifneq ($(filter device-ring,$(NATIVE_EXAMPLES)),)
define install_native_device_ring
	$(MAKE) -C native ARCH=$(1) device-ring
	install -m 0755 $(BUILD_DIR)/$(1)/native/device-ring.soyo $(2)/bin/soyo-device-ring
endef
else
define install_native_device_ring
endef
endif

define install_native_examples
	$(call install_native_hello,$(1),$(2))
	$(call install_native_rust_hello,$(1),$(2))
	$(call install_native_parent,$(1),$(2))
	$(call install_native_rust_parent,$(1),$(2))
	$(call install_native_component,$(1),$(2))
	$(call install_native_repository,$(1),$(2))
	$(call install_native_ring_io,$(1),$(2))
	$(call install_native_socket_ring,$(1),$(2))
	$(call install_native_device_ring,$(1),$(2))
endef

empty :=
space := $(empty) $(empty)
comma := ,

LA_ARCH := loongarch64
LA_TARGET := loongarch64-unknown-none
LA_CROSS_COMPILE := loongarch64-linux-gnu-
LA_COMPAT_ROOTFS_SOURCE := userland/rootfs-la
LA_COMPAT_ROOTFS := $(BUILD_DIR)/$(LA_ARCH)/compat-rootfs
LA_ROOT_KERNEL := kernel-la

RV_ARCH := riscv64
RV_TARGET := riscv64gc-unknown-none-elf
RV_CROSS_COMPILE := riscv64-linux-musl-
RV_COMPAT_ROOTFS_SOURCE := userland/rootfs-rv
RV_COMPAT_ROOTFS := $(BUILD_DIR)/$(RV_ARCH)/compat-rootfs
RV_ROOT_KERNEL := kernel-rv

ELM_MODULE_SET := drivers/Modules.toml
ELM_INTERFACE_ROOT := $(BUILD_DIR)/elm-interface
ELM_TOOL_TARGET := $(BUILD_DIR)/cargo-elm-target
ELM_TOOL := $(ELM_TOOL_TARGET)/x86_64-unknown-linux-gnu/release/cargo-elm
ELM_TOOL_INPUTS := $(wildcard tools/elm-tools/src/*.rs tools/elm-tools/Cargo.toml \
	libs/elm/src/*.rs libs/elm/src/mgr/*.rs libs/elm/macros/src/*.rs \
	libs/elm/macros/Cargo.toml libs/kernel-symbols/src/*.rs \
	libs/kernel-symbols/macros/src/*.rs) \
	libs/kernel-symbols/Cargo.toml libs/kernel-symbols/macros/Cargo.toml
ELM_KERNEL_BUILD := scripts/build-kernel-with-elm.sh

BUSYBOX_SRC := third/busybox-1.36.1
BUSYBOX_ARCHIVE := third/busybox-1.36.1.tar.gz
ENSURE_BUSYBOX := scripts/ensure-busybox.sh
BUSYBOX_SKELETON := userland/busybox-initramfs
PACK_INITRAMFS := scripts/pack-initramfs.sh
WORKLOAD_PROFILER := scripts/profile-workload-guest.sh
LEGACY_LTP_TESTCODE := userland/ltp_testcode.sh
LEGACY_LTP_SCENARIOS := userland/ltp-scenarios

ELMCTL_SRC := userland/elmctl/elmctl.c userland/elmctl/elmctl_client.c
PTHREAD_SMP_TEST_SRC := userland/tests/pthread_smp.c
ACCT_TEST_SRC := userland/tests/acct.c
SYSCALL_BENCH_SRC := userland/tests/syscall_bench.c
MM_BENCH_SRC := userland/tests/mm_fault_bench.c
RISCV_WEIGHT_SRC := userland/tests/riscv_instruction_weight_probe.c
LOONGARCH_SXE_TEST_SRC := userland/tests/loongarch_sxe.c
INIT_KEYWAIT_SRC := userland/init-keywait.c

ifeq ($(strip $(ARCH)),)
SELECTED_ARCHES := $(LA_ARCH) $(RV_ARCH)
else ifeq ($(ARCH),$(LA_ARCH))
SELECTED_ARCHES := $(LA_ARCH)
else ifeq ($(ARCH),$(RV_ARCH))
SELECTED_ARCHES := $(RV_ARCH)
else
$(error ARCH 必须为 loongarch64 或 riscv64)
endif

ifneq ($(strip $(KERNEL_MAP)),)
ifneq ($(words $(strip $(KERNEL_MAP))),1)
$(error KERNEL_MAP 路径不能包含空白字符)
endif
KERNEL_MAP_ARCHES := $(sort \
	$(if $(filter kernel-la _kernel-loongarch64 _compat-kernel-loongarch64,$(MAKECMDGOALS)),$(LA_ARCH)) \
	$(if $(filter kernel-rv _kernel-riscv64 _compat-kernel-riscv64,$(MAKECMDGOALS)),$(RV_ARCH)) \
	$(if $(filter kernel default,$(MAKECMDGOALS)),$(SELECTED_ARCHES)) \
	$(if $(filter all,$(MAKECMDGOALS)),$(LA_ARCH) $(RV_ARCH)) \
	$(if $(MAKECMDGOALS),,$(SELECTED_ARCHES)))
ifneq ($(KERNEL_MAP_ARCHES),)
ifneq ($(words $(KERNEL_MAP_ARCHES)),1)
$(error KERNEL_MAP 要求一次只构建一个架构；请使用 kernel-la/kernel-rv 或设置 ARCH)
endif
endif
endif

ifneq ($(strip $(INITRAMFS)),)
ifeq ($(words $(SELECTED_ARCHES)),2)
$(error 设置 INITRAMFS 时必须同时指定单一 ARCH)
endif
ifeq ($(wildcard $(INITRAMFS)),)
$(error INITRAMFS 文件不存在: $(INITRAMFS))
endif
EMBEDDED_FEATURE := embedded-initramfs
endif

BASE_KERNEL_FEATURES := $(strip $(FEATURES) $(EMBEDDED_FEATURE))
CARGO_FEATURES := $(subst $(space),$(comma),$(BASE_KERNEL_FEATURES))
FEATURE_ARGS := $(if $(strip $(CARGO_FEATURES)),--features "$(CARGO_FEATURES)",)
BOOTSTRAP_FEATURES := $(subst $(space),$(comma),$(strip $(FEATURES)))
BOOTSTRAP_FEATURE_ARGS := $(if $(strip $(BOOTSTRAP_FEATURES)),--features "$(BOOTSTRAP_FEATURES)",)
ELM_DRIVER_FEATURES := $(if $(filter block-bench,$(FEATURES)),block-profile,)
ELM_DRIVER_FEATURE_ARGS := $(if $(ELM_DRIVER_FEATURES),--features $(ELM_DRIVER_FEATURES),)

kernel: $(addprefix _kernel-,$(SELECTED_ARCHES))

modules: $(addprefix _modules-,$(SELECTED_ARCHES))

cargo-setup:
	@mkdir -p .cargo
	@cp cargo-config/config.toml .cargo/config.toml

$(CONFIG_FILE):
	cp configs/default.config $@

$(ELM_TOOL): $(ELM_TOOL_INPUTS)
	CARGO_TARGET_DIR=$(abspath $(ELM_TOOL_TARGET)) \
		cargo build --manifest-path tools/elm-tools/Cargo.toml \
		--target x86_64-unknown-linux-gnu --release

config: $(ELM_TOOL)
	$(ELM_TOOL) elm configure-set $(ELM_MODULE_SET) --config $(CONFIG_FILE) --mode config

oldconfig: $(ELM_TOOL)
	$(ELM_TOOL) elm configure-set $(ELM_MODULE_SET) --config $(CONFIG_FILE) --mode oldconfig

defconfig: $(ELM_TOOL)
	$(ELM_TOOL) elm configure-set $(ELM_MODULE_SET) --config $(CONFIG_FILE) --mode defconfig

define build_modules
	rm -rf $(BUILD_DIR)/$(1)/modules $(ELM_INTERFACE_ROOT)/$(2)
	env -u ELM_INTEGRATED_ARCHIVES -u ELM_BUILD_BOUND_MANIFEST -u INITRAMFS \
		$(KERNEL_INTERFACE_BUILD_ENV) cargo build -p kernel --target $(2) $(BOOTSTRAP_FEATURE_ARGS) --release
	CARGO_TARGET_DIR=$(KERNEL_INTERFACE_TARGET_DIR) \
		$(ELM_TOOL) elm profile-export $(KERNEL_INTERFACE_TARGET_DIR)/$(2)/release/kernel \
		--target $(2) --profile contest-2026 --output $(ELM_INTERFACE_ROOT)/$(2)
	env -u RUSTC_WRAPPER -u RUSTC_WORKSPACE_WRAPPER \
		ELM_KERNEL_INTERFACE_ROOT=$(abspath $(ELM_INTERFACE_ROOT)/$(2)) \
		$(ELM_TOOL) elm build-set $(ELM_MODULE_SET) --config $(CONFIG_FILE) \
		--target $(2) --output $(BUILD_DIR)/$(1)/modules $(ELM_DRIVER_FEATURE_ARGS)
endef

_modules-loongarch64: cargo-setup $(CONFIG_FILE) $(ELM_TOOL)
	$(call build_modules,$(LA_ARCH),$(LA_TARGET))

_modules-riscv64: cargo-setup $(CONFIG_FILE) $(ELM_TOOL)
	$(call build_modules,$(RV_ARCH),$(RV_TARGET))

define build_kernel
	mkdir -p $(BUILD_DIR)/$(1)
	ELM_BIND_MODULES=$(4) INITRAMFS=$(INITRAMFS) \
		KERNEL_LINK_MAP="$(if $(strip $(KERNEL_MAP)),$(abspath $(KERNEL_MAP)))" \
		KERNEL_LINK_OUTPUT="$(if $(strip $(KERNEL_MAP)),$(abspath $(BUILD_DIR)/$(1)/kernel))" \
		KERNEL_LINK_SOURCE="$(if $(strip $(KERNEL_MAP)),$(abspath $(CARGO_TARGET_DIR)/$(2)/release/kernel))" \
		KERNEL_LINK_TARGET="$(if $(strip $(KERNEL_MAP)),$(2))" \
		KERNEL_LINK_ROOT_OUTPUT="$(if $(and $(strip $(KERNEL_MAP)),$(strip $(KERNEL_PUBLISH_OUTPUT))),$(abspath $(KERNEL_PUBLISH_OUTPUT)))" \
		$(KCSAN_WRAPPER_ENV) $(ELM_KERNEL_BUILD) $(BUILD_DIR)/$(1)/modules/modules.manifest \
		$(BUILD_DIR)/$(1)/modules/integrated.archives \
		cargo build -p kernel --target $(2) $(FEATURE_ARGS) --release
	$(if $(strip $(KERNEL_MAP)),test -s $(BUILD_DIR)/$(1)/kernel,cp $(CARGO_TARGET_DIR)/$(2)/release/kernel $(BUILD_DIR)/$(1)/kernel)
	@echo "kernel image: $(BUILD_DIR)/$(1)/kernel"
endef

_kernel-loongarch64: _modules-loongarch64
	$(call build_kernel,$(LA_ARCH),$(LA_TARGET),$(LA_CROSS_COMPILE),0)

_kernel-riscv64: _modules-riscv64
	$(call build_kernel,$(RV_ARCH),$(RV_TARGET),$(RV_CROSS_COMPILE),0)

modules_install:
	@test -n "$(ARCH)" || { echo "modules_install 要求 ARCH" >&2; exit 2; }
	@test -n "$(INSTALL_MOD_PATH)" || { echo "modules_install 要求 INSTALL_MOD_PATH" >&2; exit 2; }
	$(MAKE) modules ARCH=$(ARCH) CONFIG_FILE=$(CONFIG_FILE) FEATURES="$(FEATURES)"
	mkdir -p $(INSTALL_MOD_PATH)/lib/elm
	install -m 0644 $(BUILD_DIR)/$(ARCH)/modules/modules.manifest $(INSTALL_MOD_PATH)/lib/elm/
	find $(BUILD_DIR)/$(ARCH)/modules -maxdepth 1 -type f -name '*.eki' \
		-exec install -m 0644 {} $(INSTALL_MOD_PATH)/lib/elm/ \;

define build_busybox
	$(ENSURE_BUSYBOX)
	rm -rf $(BUILD_DIR)/$(1)/busybox-build $(BUILD_DIR)/$(1)/busybox-rootfs
	mkdir -p $(BUILD_DIR)/$(1)/busybox-build $(BUILD_DIR)/$(1)/busybox-rootfs
	$(MAKE) -C $(BUSYBOX_SRC) O=$(abspath $(BUILD_DIR)/$(1)/busybox-build) \
		CROSS_COMPILE=$(2) defconfig
	sed -i 's/.*CONFIG_STATIC.*/CONFIG_STATIC=y/' $(BUILD_DIR)/$(1)/busybox-build/.config
	sed -i 's/.*CONFIG_PIE.*/CONFIG_PIE=y/' $(BUILD_DIR)/$(1)/busybox-build/.config
	sed -i 's/^CONFIG_TC=.*/# CONFIG_TC is not set/' $(BUILD_DIR)/$(1)/busybox-build/.config
	yes '' | $(MAKE) -C $(BUSYBOX_SRC) O=$(abspath $(BUILD_DIR)/$(1)/busybox-build) \
		CROSS_COMPILE=$(2) oldconfig
	$(MAKE) -C $(BUSYBOX_SRC) O=$(abspath $(BUILD_DIR)/$(1)/busybox-build) \
		CROSS_COMPILE=$(2) -j$(JOBS)
	$(MAKE) -C $(BUSYBOX_SRC) O=$(abspath $(BUILD_DIR)/$(1)/busybox-build) \
		CROSS_COMPILE=$(2) CONFIG_PREFIX=$(abspath $(BUILD_DIR)/$(1)/busybox-rootfs) install
	cp -a $(BUSYBOX_SKELETON)/. $(BUILD_DIR)/$(1)/busybox-rootfs/
	ln -sfn bin/busybox $(BUILD_DIR)/$(1)/busybox-rootfs/init
	chmod +x $(BUILD_DIR)/$(1)/busybox-rootfs/etc/init.d/rcS
	mkdir -p $(BUILD_DIR)/$(1)/busybox-rootfs/dev $(BUILD_DIR)/$(1)/busybox-rootfs/proc \
		$(BUILD_DIR)/$(1)/busybox-rootfs/sys $(BUILD_DIR)/$(1)/busybox-rootfs/tmp
	-$(2)strip $(BUILD_DIR)/$(1)/busybox-rootfs/bin/busybox
	$(PACK_INITRAMFS) $(BUILD_DIR)/$(1)/busybox-rootfs $(BUILD_DIR)/$(1)/initramfs.cpio
	@echo "initramfs image: $(BUILD_DIR)/$(1)/initramfs.cpio"
endef

busybox: $(addprefix _busybox-,$(SELECTED_ARCHES))

_busybox-loongarch64: $(ENSURE_BUSYBOX) $(BUSYBOX_ARCHIVE) $(PACK_INITRAMFS)
	$(call build_busybox,$(LA_ARCH),$(LA_CROSS_COMPILE))

_busybox-riscv64: $(ENSURE_BUSYBOX) $(BUSYBOX_ARCHIVE) $(PACK_INITRAMFS)
	$(call build_busybox,$(RV_ARCH),$(RV_CROSS_COMPILE))

define build_elm_user_tools
	rm -rf $(BUILD_DIR)/$(1)/elm-user
	mkdir -p $(BUILD_DIR)/$(1)/elm-user $(2)/bin
	$(3)gcc -std=c11 -static -Os -Wall -Wextra -Iuserland/elmctl/include \
		$(ELMCTL_SRC) -o $(BUILD_DIR)/$(1)/elm-user/elmctl
	-$(3)strip $(BUILD_DIR)/$(1)/elm-user/elmctl
	install -m 0755 $(BUILD_DIR)/$(1)/elm-user/elmctl $(2)/bin/
	$(3)gcc -std=c11 -static -Os -Wall -Wextra \
		$(INIT_KEYWAIT_SRC) -o $(BUILD_DIR)/$(1)/elm-user/init-keywait
	-$(3)strip $(BUILD_DIR)/$(1)/elm-user/init-keywait
	install -m 0755 $(BUILD_DIR)/$(1)/elm-user/init-keywait $(2)/bin/
endef

define build_smp_user_tests
	@if [ -n "$(filter smp-tests,$(FEATURES))" ]; then \
		rm -rf $(BUILD_DIR)/$(1)/smp-user; \
		mkdir -p $(BUILD_DIR)/$(1)/smp-user $(2)/bin; \
		$(3)gcc -std=c11 -static -O2 -Wall -Wextra -Werror -pthread \
			$(PTHREAD_SMP_TEST_SRC) -o $(BUILD_DIR)/$(1)/smp-user/pthread-smp-test; \
		$(3)gcc -std=c11 -static -O2 -Wall -Wextra -Werror -pthread \
			$(ACCT_TEST_SRC) -o $(BUILD_DIR)/$(1)/smp-user/acct-test; \
		$(3)strip $(BUILD_DIR)/$(1)/smp-user/pthread-smp-test || true; \
		$(3)strip $(BUILD_DIR)/$(1)/smp-user/acct-test || true; \
		install -m 0755 $(BUILD_DIR)/$(1)/smp-user/pthread-smp-test $(2)/bin/; \
		install -m 0755 $(BUILD_DIR)/$(1)/smp-user/acct-test $(2)/bin/; \
	fi
endef

define build_loongarch_sxe_tests
	@if [ "$(1)" = "$(LA_ARCH)" ] && [ -n "$(filter lazy-sxe-tests,$(FEATURES))" ]; then \
		rm -rf $(BUILD_DIR)/$(1)/lazy-sxe-user; \
		mkdir -p $(BUILD_DIR)/$(1)/lazy-sxe-user $(2)/bin; \
		$(3)gcc -std=c11 -static -O2 -Wall -Wextra -Werror -mlsx -mno-lasx \
			-fno-tree-vectorize -fno-tree-slp-vectorize \
			$(LOONGARCH_SXE_TEST_SRC) \
			-o $(BUILD_DIR)/$(1)/lazy-sxe-user/loongarch-sxe-test; \
		$(3)strip $(BUILD_DIR)/$(1)/lazy-sxe-user/loongarch-sxe-test || true; \
		install -m 0755 $(BUILD_DIR)/$(1)/lazy-sxe-user/loongarch-sxe-test $(2)/bin/; \
	fi
endef

define build_syscall_benchmark
	@if [ "$(1)" = "$(RV_ARCH)" ] && [ "$(TEST_MODE)" = "syscall-bench" ]; then \
		rm -rf $(BUILD_DIR)/$(1)/syscall-bench; \
		mkdir -p $(BUILD_DIR)/$(1)/syscall-bench $(2)/bin; \
		$(3)gcc -std=c11 -static -fno-pie -no-pie -O2 -Wall -Wextra -Werror \
			$(SYSCALL_BENCH_SRC) -o $(BUILD_DIR)/$(1)/syscall-bench/syscall-bench.elf; \
		install -m 0755 $(BUILD_DIR)/$(1)/syscall-bench/syscall-bench.elf \
			$(2)/bin/syscall-bench; \
		$(3)strip $(2)/bin/syscall-bench || true; \
		printf '%s %s %s %s\n' \
			'$(SYSCALL_BENCH_ITERATIONS)' '$(SYSCALL_BENCH_REPEATS)' \
			'$(SYSCALL_BENCH_CASE)' '$(SYSCALL_BENCH_WARMUP)' \
			>$(2)/etc/mygo-syscall-bench-args; \
	fi
endef

define build_mm_benchmark
	@if [ "$(1)" = "$(RV_ARCH)" ] && [ "$(TEST_MODE)" = "mm-bench" ]; then \
		rm -rf $(BUILD_DIR)/$(1)/mm-bench; \
		mkdir -p $(BUILD_DIR)/$(1)/mm-bench $(2)/bin; \
		$(3)gcc -std=c11 -static -fno-pie -no-pie -O2 -Wall -Wextra -Werror -pthread \
			$(MM_BENCH_SRC) -o $(BUILD_DIR)/$(1)/mm-bench/mm-fault-bench.elf; \
		install -m 0755 $(BUILD_DIR)/$(1)/mm-bench/mm-fault-bench.elf \
			$(2)/bin/mm-fault-bench; \
		$(3)strip $(2)/bin/mm-fault-bench || true; \
		printf '%s %s %s %s\n' \
			'$(MM_BENCH_CASE)' '$(MM_BENCH_PAGES)' \
			'$(MM_BENCH_THREADS)' '$(MM_BENCH_REPEATS)' \
			>$(2)/etc/mygo-mm-bench-args; \
	fi
endef

define build_riscv_instruction_weight_probe
	@if [ "$(1)" = "$(RV_ARCH)" ] && [ "$(TEST_MODE)" = "instruction-weight" ]; then \
		rm -rf $(BUILD_DIR)/$(1)/instruction-weight; \
		mkdir -p $(BUILD_DIR)/$(1)/instruction-weight $(2)/bin; \
		$(3)gcc -std=c11 -static -fno-pie -no-pie -O2 -Wall -Wextra -Werror \
			$(RISCV_WEIGHT_SRC) \
			-o $(BUILD_DIR)/$(1)/instruction-weight/riscv-instruction-weight.elf; \
		install -m 0755 \
			$(BUILD_DIR)/$(1)/instruction-weight/riscv-instruction-weight.elf \
			$(2)/bin/riscv-instruction-weight; \
		$(3)strip $(2)/bin/riscv-instruction-weight || true; \
		printf '%s %s %s %s\n' \
			'$(RISCV_WEIGHT_BASE_BLOCKS)' '$(RISCV_WEIGHT_ROUNDS)' \
			'$(RISCV_WEIGHT_CASE)' '$(RISCV_WEIGHT_RUN_ID)' \
			>$(2)/etc/mygo-riscv-instruction-weight-args; \
	fi
endef

define prepare_compat_rootfs
	$(MAKE) _busybox-$(1)
	rm -rf $(2)
	mkdir -p $(2)
	cp -a $(BUILD_DIR)/$(1)/busybox-rootfs/. $(2)/
	mkdir -p $(2)/etc $(2)/tmp
	cp -a $(3)/etc/. $(2)/etc/
	mkdir -p $(2)/etc/ltp-scenarios
	cp -a $(LEGACY_LTP_SCENARIOS)/. $(2)/etc/ltp-scenarios/
	install -m 0755 $(LEGACY_LTP_TESTCODE) $(2)/etc/ltp_testcode.sh
	install -m 0755 $(3)/etc/init.d/rcS $(2)/etc/init.d/rcS
	install -m 0755 $(3)/etc/init.d/test.sh $(2)/etc/init.d/test.sh
	install -m 0755 $(3)/etc/init.d/judge.sh $(2)/etc/init.d/judge.sh
	printf '%s\n' '$(TEST_MODE)' >$(2)/etc/mygo-test-mode
	printf '%s\n' '$(TEST_WORKLOAD)' >$(2)/etc/mygo-test-workload
	printf '%s\n' '$(PROFILE_MODE)' >$(2)/etc/mygo-profile-mode
	printf '%s\n' '$(PROFILE_PRESET)' >$(2)/etc/mygo-profile-preset
	printf '%s\n' '$(PROFILE_SAMPLE_HZ)' >$(2)/etc/mygo-profile-sample-hz
	printf '%s\n' '$(PROFILE_WORKLOAD)' >$(2)/etc/mygo-profile-workload
	@if [ -n "$(filter performance-profile,$(FEATURES))" ]; then \
		install -m 0755 $(WORKLOAD_PROFILER) $(2)/bin/profile-workload-guest; \
		if [ -n "$(PROFILE_PHASE_RULES)" ]; then \
			install -m 0644 "$(PROFILE_PHASE_RULES)" $(2)/etc/mygo-profile-phases; \
		else \
			rm -f $(2)/etc/mygo-profile-phases; \
		fi; \
	fi
	mkdir -p $(2)/lib/elm
	rm -f $(2)/lib/elm/*
	$(call build_elm_user_tools,$(1),$(2),$(5))
	$(call build_smp_user_tests,$(1),$(2),$(5))
	$(call build_loongarch_sxe_tests,$(1),$(2),$(5))
	$(call install_native_examples,$(1),$(2))
	@if [ -n "$(strip $(NATIVE_EXAMPLE_COMMANDS))" ]; then \
		printf '%s\n' $(NATIVE_EXAMPLE_COMMANDS) >$(2)/etc/mygo-native-examples; \
	else \
		rm -f $(2)/etc/mygo-native-examples; \
	fi
	$(call build_syscall_benchmark,$(1),$(2),$(5))
	$(call build_mm_benchmark,$(1),$(2),$(5))
	$(call build_riscv_instruction_weight_probe,$(1),$(2),$(5))
	install -m 0644 $(BUILD_DIR)/$(1)/modules/modules.manifest $(2)/lib/elm/
	find $(BUILD_DIR)/$(1)/modules -maxdepth 1 -type f -name '*.eki' \
		-exec install -m 0644 {} $(2)/lib/elm/ \;
	$(PACK_INITRAMFS) $(2) $(BUILD_DIR)/$(1)/compat-initramfs.cpio
endef

kernel-la: _modules-loongarch64 $(PACK_INITRAMFS)
	$(call prepare_compat_rootfs,$(LA_ARCH),$(LA_COMPAT_ROOTFS),$(LA_COMPAT_ROOTFS_SOURCE),$(LA_TARGET),$(LA_CROSS_COMPILE))
	$(MAKE) _compat-kernel-loongarch64 $(if $(strip $(KERNEL_MAP)),KERNEL_PUBLISH_OUTPUT=$(abspath $(LA_ROOT_KERNEL)))
	$(if $(strip $(KERNEL_MAP)),test -s $(LA_ROOT_KERNEL),cp $(BUILD_DIR)/$(LA_ARCH)/kernel $(LA_ROOT_KERNEL))

_compat-kernel-loongarch64:
	$(eval override INITRAMFS := $(abspath $(BUILD_DIR)/$(LA_ARCH)/compat-initramfs.cpio))
	$(eval override EMBEDDED_FEATURE := embedded-initramfs)
	$(eval override BASE_KERNEL_FEATURES := $(strip $(FEATURES) embedded-initramfs))
	$(eval override CARGO_FEATURES := $(subst $(space),$(comma),$(BASE_KERNEL_FEATURES)))
	$(eval override FEATURE_ARGS := --features "$(CARGO_FEATURES)")
	$(call build_kernel,$(LA_ARCH),$(LA_TARGET),$(LA_CROSS_COMPILE),1)

syscall-bench-rv:
	$(MAKE) kernel-rv TEST_MODE=syscall-bench \
		KERNEL_MAP=$(abspath $(BUILD_DIR)/$(RV_ARCH)/kernel.map)

mm-bench-rv:
	$(MAKE) kernel-rv TEST_MODE=mm-bench \
		MM_BENCH_CASE='$(MM_BENCH_CASE)' MM_BENCH_PAGES='$(MM_BENCH_PAGES)' \
		MM_BENCH_THREADS='$(MM_BENCH_THREADS)' MM_BENCH_REPEATS='$(MM_BENCH_REPEATS)' \
		KERNEL_MAP=$(abspath $(BUILD_DIR)/$(RV_ARCH)/kernel.map)

instruction-weight-rv:
	$(MAKE) kernel-rv TEST_MODE=instruction-weight \
		RISCV_WEIGHT_BASE_BLOCKS='$(RISCV_WEIGHT_BASE_BLOCKS)' \
		RISCV_WEIGHT_ROUNDS='$(RISCV_WEIGHT_ROUNDS)' \
		RISCV_WEIGHT_CASE='$(RISCV_WEIGHT_CASE)' \
		RISCV_WEIGHT_RUN_ID='$(RISCV_WEIGHT_RUN_ID)' \
		KERNEL_MAP=$(abspath $(BUILD_DIR)/$(RV_ARCH)/kernel.map)

kernel-rv: _modules-riscv64 $(PACK_INITRAMFS)
	$(call prepare_compat_rootfs,$(RV_ARCH),$(RV_COMPAT_ROOTFS),$(RV_COMPAT_ROOTFS_SOURCE),$(RV_TARGET),$(RV_CROSS_COMPILE))
	$(MAKE) _compat-kernel-riscv64 $(if $(strip $(KERNEL_MAP)),KERNEL_PUBLISH_OUTPUT=$(abspath $(RV_ROOT_KERNEL)))
	$(if $(strip $(KERNEL_MAP)),test -s $(RV_ROOT_KERNEL),cp $(BUILD_DIR)/$(RV_ARCH)/kernel $(RV_ROOT_KERNEL))

kcsan-la:
	$(MAKE) kernel-la ARCH=$(LA_ARCH) \
		FEATURES="$(strip $(filter-out kcsan,$(FEATURES)) kcsan)" \
		KCSAN_BUILD=1 \
		BUILD_DIR=$(KCSAN_BUILD_DIR) CARGO_TARGET_DIR=$(KCSAN_TARGET_DIR) \
		KERNEL_MAP=$(KCSAN_BUILD_DIR)/$(LA_ARCH)/kernel.map \
		LA_ROOT_KERNEL=$(abspath kernel-la-kcsan) CARGO_PROFILE_RELEASE_DEBUG=2

kcsan-rv:
	$(MAKE) kernel-rv ARCH=$(RV_ARCH) \
		FEATURES="$(strip $(filter-out kcsan,$(FEATURES)) kcsan)" \
		KCSAN_BUILD=1 \
		BUILD_DIR=$(KCSAN_BUILD_DIR) CARGO_TARGET_DIR=$(KCSAN_TARGET_DIR) \
		KERNEL_MAP=$(KCSAN_BUILD_DIR)/$(RV_ARCH)/kernel.map \
		RV_ROOT_KERNEL=$(abspath kernel-rv-kcsan) CARGO_PROFILE_RELEASE_DEBUG=2

_compat-kernel-riscv64:
	$(eval override INITRAMFS := $(abspath $(BUILD_DIR)/$(RV_ARCH)/compat-initramfs.cpio))
	$(eval override BASE_KERNEL_FEATURES := $(strip $(FEATURES) embedded-initramfs))
	$(eval override CARGO_FEATURES := $(subst $(space),$(comma),$(BASE_KERNEL_FEATURES)))
	$(eval override FEATURE_ARGS := --features "$(CARGO_FEATURES)")
	$(call build_kernel,$(RV_ARCH),$(RV_TARGET),$(RV_CROSS_COMPILE),1)

all: kernel-la kernel-rv

native-hello-la:
	$(MAKE) -C native ARCH=$(LA_ARCH) hello

native-hello-rv:
	$(MAKE) -C native ARCH=$(RV_ARCH) hello

native-rust-hello-la:
	$(MAKE) -C native ARCH=$(LA_ARCH) rust-hello

native-rust-hello-rv:
	$(MAKE) -C native ARCH=$(RV_ARCH) rust-hello

native-parent-la:
	$(MAKE) -C native ARCH=$(LA_ARCH) native-parent

native-parent-rv:
	$(MAKE) -C native ARCH=$(RV_ARCH) native-parent

native-rust-parent-la:
	$(MAKE) -C native ARCH=$(LA_ARCH) native-rust-parent

native-rust-parent-rv:
	$(MAKE) -C native ARCH=$(RV_ARCH) native-rust-parent

native-component-la:
	$(MAKE) -C native ARCH=$(LA_ARCH) component

native-component-rv:
	$(MAKE) -C native ARCH=$(RV_ARCH) component

native-repository-la:
	$(MAKE) -C native ARCH=$(LA_ARCH) repository-client

native-repository-rv:
	$(MAKE) -C native ARCH=$(RV_ARCH) repository-client

native-ring-io-la:
	$(MAKE) -C native ARCH=$(LA_ARCH) ring-io

native-ring-io-rv:
	$(MAKE) -C native ARCH=$(RV_ARCH) ring-io

native-socket-ring-la:
	$(MAKE) -C native ARCH=$(LA_ARCH) socket-ring

native-socket-ring-rv:
	$(MAKE) -C native ARCH=$(RV_ARCH) socket-ring

native-device-ring-la:
	$(MAKE) -C native ARCH=$(LA_ARCH) device-ring

native-device-ring-rv:
	$(MAKE) -C native ARCH=$(RV_ARCH) device-ring

clean:
	cargo clean
	rm -rf $(BUILD_DIR)/loongarch64 $(BUILD_DIR)/riscv64 $(ELM_INTERFACE_ROOT) \
		$(ELM_TOOL_TARGET) $(KCSAN_BUILD_DIR) $(KCSAN_TARGET_DIR)
	rm -f $(LA_ROOT_KERNEL) $(RV_ROOT_KERNEL) \
		$(LA_ROOT_KERNEL).lock $(RV_ROOT_KERNEL).lock \
		kernel-la-kcsan kernel-rv-kcsan kernel-la-kcsan.lock kernel-rv-kcsan.lock
