#!/bin/sh

PATH=/sbin:/bin:/usr/sbin:/usr/bin
export PATH

maybe_enter_shell() {
    key_file="/tmp/init-key.$$"

    echo "[init] press Ctrl+C within 3 seconds to enter shell"
    trap 'echo "[init] Ctrl+C detected, entering shell"; exec /bin/sh -i' INT
    old_stty="$(stty -g < /dev/console 2>/dev/null || true)"
    stty raw -echo < /dev/console 2>/dev/null || true
    ( dd bs=1 count=1 of="$key_file" < /dev/console 2>/dev/null ) &
    key_pid=$!
    sleep 3
    if kill -0 "$key_pid" 2>/dev/null; then
        kill "$key_pid" 2>/dev/null || true
        wait "$key_pid" 2>/dev/null || true
    else
        wait "$key_pid" 2>/dev/null || true
    fi
    [ -n "$old_stty" ] && stty "$old_stty" < /dev/console 2>/dev/null || true
    trap - INT

    key_code="$(od -An -tu1 "$key_file" 2>/dev/null | tr -d '[:space:]')"
    if [ "$key_code" = "3" ]; then
        rm -f "$key_file"
        echo "[init] Ctrl+C detected, entering shell"
        exec /bin/sh -i
    fi
    rm -f "$key_file"
}

# 默认不跳过测试；需要临时跳过时，可通过内核命令行黑名单参数显式配置。
GLIBC_BLACKLIST=""
MUSL_BLACKLIST=""
BUILDSTORM_ENABLED=1
TEST_MODE="$(cat /etc/mygo-test-mode 2>/dev/null || echo default)"
TEST_WORKLOAD="$(cat /etc/mygo-test-workload 2>/dev/null || true)"
PROFILE_WORKLOAD="$(cat /etc/mygo-profile-workload 2>/dev/null || true)"
PROFILE_MODE="$(cat /etc/mygo-profile-mode 2>/dev/null || echo sample)"
PROFILE_PRESET="$(cat /etc/mygo-profile-preset 2>/dev/null || echo all)"
PROFILE_SAMPLE_HZ="$(cat /etc/mygo-profile-sample-hz 2>/dev/null || echo 250)"

if [ -r /sys/kernel/cmdline ]; then
    for arg in $(cat /sys/kernel/cmdline 2>/dev/null); do
        case "$arg" in
            test_blacklist=none)
                GLIBC_BLACKLIST=""
                MUSL_BLACKLIST=""
                ;;
            test_blacklist=*)
                blacklist="$(printf '%s' "${arg#test_blacklist=}" | tr ',' ' ')"
                GLIBC_BLACKLIST="$blacklist"
                MUSL_BLACKLIST="$blacklist"
                ;;
            glibc_blacklist=none) GLIBC_BLACKLIST="" ;;
            glibc_blacklist=*)
                GLIBC_BLACKLIST="$(printf '%s' "${arg#glibc_blacklist=}" | tr ',' ' ')"
                ;;
            musl_blacklist=none) MUSL_BLACKLIST="" ;;
            musl_blacklist=*)
                MUSL_BLACKLIST="$(printf '%s' "${arg#musl_blacklist=}" | tr ',' ' ')"
                ;;
            buildstorm=1|buildstorm=on|buildstorm=yes)
                BUILDSTORM_ENABLED=1
                ;;
            buildstorm=0|buildstorm=off|buildstorm=no)
                BUILDSTORM_ENABLED=0
                ;;
        esac
    done
fi

_is_blacklisted() {
    libc_name="$1"
    test_name="$2"
    case "$libc_name" in
        glibc) blacklist="$GLIBC_BLACKLIST" ;;
        musl) blacklist="$MUSL_BLACKLIST" ;;
        *) blacklist="" ;;
    esac

    for _bl in $blacklist; do
        case "$test_name" in
            *${_bl}*) return 0 ;;
        esac
    done
    return 1
}

_test_setup() {
    if [ -e /dev/tty ] && [ ! -L /dev/tty ]; then
        mv /dev/tty /dev/tty.real
    fi
    ln -sf /dev/null /dev/tty
}

_test_teardown() {
    rm -f /dev/tty
    [ -e /dev/tty.real ] && mv /dev/tty.real /dev/tty
}

setup_basic_mount_loop() {
    img="/tmp/basic-vda2.fat"
    state="/tmp/basic-vda2.loop"
    loopdev=""

    if [ -f "$state" ] || [ -L /dev/vda2 ]; then
        cleanup_basic_mount_loop >/dev/null 2>&1 || true
    fi
    rm -f "$img"

    if ! command -v losetup >/dev/null 2>&1 || ! command -v mkfs.vfat >/dev/null 2>&1; then
        echo "[init] basic mount loop skipped: losetup or mkfs.vfat missing"
        return 0
    fi

    truncate -s 64M "$img" 2>/dev/null || dd if=/dev/zero of="$img" bs=1M count=64 2>/dev/null || {
        echo "[init] basic mount loop skipped: cannot create image"
        return 0
    }
    mkfs.vfat -F 32 "$img" >/dev/null 2>&1 || mkfs.vfat "$img" >/dev/null 2>&1 || {
        echo "[init] basic mount loop skipped: mkfs.vfat failed"
        rm -f "$img"
        return 0
    }

    loopdev="$(losetup -f 2>/dev/null || true)"
    if [ -z "$loopdev" ]; then
        echo "[init] basic mount loop skipped: no free loop device"
        rm -f "$img"
        return 0
    fi
    losetup "$loopdev" "$img" >/dev/null 2>&1 || {
        echo "[init] basic mount loop skipped: losetup failed"
        rm -f "$img"
        return 0
    }

    ln -sf "$loopdev" /dev/vda2 2>/dev/null || {
        echo "[init] basic mount loop skipped: cannot link /dev/vda2"
        losetup -d "$loopdev" >/dev/null 2>&1 || true
        rm -f "$img"
        return 0
    }

    echo "$loopdev" >"$state" 2>/dev/null || true
    echo "[init] basic mount loop ready: /dev/vda2 -> $loopdev"
}

cleanup_basic_mount_loop() {
    img="/tmp/basic-vda2.fat"
    state="/tmp/basic-vda2.loop"
    loopdev=""

    if [ ! -f "$state" ] && [ ! -L /dev/vda2 ]; then
        return 0
    fi

    if [ -f "$state" ]; then
        loopdev="$(cat "$state" 2>/dev/null || true)"
    fi
    if [ -z "$loopdev" ] && [ -L /dev/vda2 ]; then
        loopdev="$(readlink /dev/vda2 2>/dev/null || true)"
    fi

    rm -f /dev/vda2 "$state"
    if [ -n "$loopdev" ]; then
        losetup -d "$loopdev" >/dev/null 2>&1 || true
    fi
    rm -f "$img"
}

run_tmpfs_test_script() {
    script="$1"
    name="${script##*/}"
    dir="${script%/*}"
    libc_name="${dir##*/}"
    test_name="${name%_testcode.sh}"
    oldpwd="$PWD"
    work="/tmp/${test_name}-${libc_name}-$$"
    mounted=0
    old_ld_set=0
    old_ld="${LD_LIBRARY_PATH:-}"
    script_to_run="$script"
    lmbench_var_tmp_mounted=0
    lmbench_hello=""

    rm -rf "$work"
    if ! mkdir -p "$work"; then
        echo "[init] $test_name tmpfs setup failed: cannot create $work"
        cd "$dir" || return 0
        /bin/sh "$script"
        cd "$oldpwd" || true
        return 0
    fi

    if mount -t tmpfs tmpfs "$work" 2>/dev/null; then
        mounted=1
        echo "[init] $test_name uses tmpfs workdir: $work"
    else
        echo "[init] $test_name uses /tmp workdir: $work"
    fi

    for item in "$dir"/* "$dir"/.[!.]* "$dir"/..?*; do
        [ -e "$item" ] || continue
        base="${item##*/}"
        case "$base" in
            .|..) continue ;;
        esac
        ln -sf "$item" "$work/$base" 2>/dev/null || true
    done

    if [ "$test_name" = "lmbench" ]; then
        mkdir -p /var/tmp 2>/dev/null || true
        if mount -t tmpfs tmpfs /var/tmp 2>/dev/null; then
            lmbench_var_tmp_mounted=1
            echo "[init] lmbench uses tmpfs /var/tmp"
        fi
        lmbench_hello="$work/hello"
        rm -f "$lmbench_hello"
        {
            echo '#!/bin/sh'
            echo "exec \"$work/lmbench_all\" hello \"\$@\""
        } >"$lmbench_hello" 2>/dev/null || true
        chmod +x "$lmbench_hello" 2>/dev/null || true
    fi

    [ "${LD_LIBRARY_PATH+x}" = "x" ] && old_ld_set=1
    if [ -d "$dir/lib" ]; then
        case ":${LD_LIBRARY_PATH:-}:" in
            *:"$dir/lib":*) ;;
            *) LD_LIBRARY_PATH="$dir/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" ;;
        esac
        export LD_LIBRARY_PATH
    fi

    if cd "$work"; then
        /bin/sh "$script_to_run"
    else
        echo "[init] $test_name tmpfs setup failed: cannot enter $work"
    fi

    if [ "$old_ld_set" = "1" ]; then
        LD_LIBRARY_PATH="$old_ld"
        export LD_LIBRARY_PATH
    else
        unset LD_LIBRARY_PATH
    fi
    if [ "$lmbench_var_tmp_mounted" = "1" ]; then
        umount /var/tmp 2>/dev/null || true
    fi
    [ -n "$lmbench_hello" ] && rm -f "$lmbench_hello" /tmp/hello
    cd "$oldpwd" || true
    if [ "$mounted" = "1" ]; then
        umount "$work" 2>/dev/null || true
    fi
    rm -rf "$work"
}

run_busybox_test_script() {
    script="$1"
    dir="${script%/*}"
    patched="/tmp/busybox-testcode.$$"

    if sed 's/if \[\[ $RTN -ne 0 && "$line" != "false" \]\] ;then/if [ "$RTN" -ne 0 ] \&\& [ "$line" != "false" ]; then/' "$script" >"$patched" 2>/dev/null; then
        chmod +x "$patched" 2>/dev/null || true
        cd "$dir" || return 0
        /bin/sh "$patched"
        rm -f "$patched"
    else
        rm -f "$patched"
        cd "$dir" || return 0
        /bin/sh "$script"
    fi
}

run_basic_test_script() {
    script="$1"
    dir="${script%/*}"
    patched="/tmp/basic-testcode.$$"

    if sed 's#\./run-all\.sh#/bin/sh ./run-all.sh#g' "$script" >"$patched" 2>/dev/null; then
        cd "$dir" || return 0
        /bin/sh "$patched"
        status=$?
        rm -f "$patched"
        return "$status"
    fi
    cd "$dir" || return 0
    /bin/sh "$script"
}

prepare_libctest_dso_links() {
    dir="$1"

    [ -d "$dir/lib" ] || return 0
    for so in dlopen_dso.so tls_align_dso.so tls_get_new-dtv_dso.so tls_init_dso.so; do
        [ -e "$dir/$so" ] && continue
        [ -e "$dir/lib/$so" ] || continue
        # 部分测试盘只把 libctest 的 dlopen 依赖放在 lib/，而用例使用 ./xxx.so。
        ln -sf "lib/$so" "$dir/$so" 2>/dev/null || true
    done
}

CHROOT_TEST_MOUNTS=""

_is_path_mounted() {
    target="$1"
    while read -r _source mounted_at _rest; do
        [ "$mounted_at" = "$target" ] && return 0
    done < /proc/mounts
    return 1
}

_mount_chroot_fs() {
    source="$1"
    fs_type="$2"
    target="$3"

    if _is_path_mounted "$target"; then
        return 0
    fi
    if ! mount -t "$fs_type" "$source" "$target" 2>/dev/null; then
        echo "[init] failed to mount $fs_type at $target"
        return 1
    fi
    CHROOT_TEST_MOUNTS="$target $CHROOT_TEST_MOUNTS"
}

_chroot_mount_owned() {
    target="$1"
    case " $CHROOT_TEST_MOUNTS " in
        *" $target "*) return 0 ;;
    esac
    return 1
}

cleanup_chroot_test_root() {
    for target in /mnt/run /mnt/dev/shm /mnt/dev /mnt/sys /mnt/proc; do
        if _chroot_mount_owned "$target"; then
            umount "$target" 2>/dev/null || \
                echo "[init] failed to unmount chroot filesystem $target"
        fi
    done
    CHROOT_TEST_MOUNTS=""
}

prepare_chroot_test_root() {
    CHROOT_TEST_MOUNTS=""
    mkdir -p /mnt/proc /mnt/sys /mnt/dev /mnt/run /mnt/tmp || return 1

    _mount_chroot_fs proc proc /mnt/proc || return 1
    _mount_chroot_fs sysfs sysfs /mnt/sys || return 1
    _mount_chroot_fs devtmpfs devtmpfs /mnt/dev || return 1

    mkdir -p /mnt/dev/pts /mnt/dev/shm || return 1
    _mount_chroot_fs tmpfs tmpfs /mnt/dev/shm || return 1
    _mount_chroot_fs tmpfs tmpfs /mnt/run || return 1
}

run_chroot_test_script() {
    script="$1"
    name="${script##*/}"
    dir="${script%/*}"
    libc_name="${dir##*/}"
    runner="$script"
    temporary_runner=""
    temporary_rules=""
    runner_arg1=""
    runner_arg2=""
    runner_arg3=""

    case "$script" in
        /mnt/*) ;;
        *)
            echo "[init] refusing chroot test outside /mnt: $script"
            return 1
            ;;
    esac

    if ! prepare_chroot_test_root; then
        cleanup_chroot_test_root
        echo "[init] $name chroot setup failed"
        return 1
    fi

    if [ "$name" != "cagent_testcode.sh" ] && \
        [ -n "$PROFILE_WORKLOAD" ] && [ "$name" = "$PROFILE_WORKLOAD" ] && \
        [ -x /bin/profile-workload-guest ]; then
        temporary_runner="/mnt/tmp/.mygo-profile-workload"
        if ! cp /bin/profile-workload-guest "$temporary_runner"; then
            echo "[init] failed to install workload profile runner"
            cleanup_chroot_test_root
            return 1
        fi
        chmod 0755 "$temporary_runner" 2>/dev/null || true
        runner="$temporary_runner"
        runner_arg1="${name%_testcode.sh}"
        runner_arg2="${script#/mnt}"
        if [ -r /etc/mygo-profile-phases ]; then
            temporary_rules="/mnt/tmp/.mygo-profile-phases"
            cp /etc/mygo-profile-phases "$temporary_rules" || return 1
            runner_arg3="${temporary_rules#/mnt}"
        fi
    fi

    inside_runner="${runner#/mnt}"
    inside_dir="${dir#/mnt}"
    chroot_shell=/bin/sh
    if [ "$name" = "cagent_testcode.sh" ]; then
        if [ ! -x /mnt/bin/bash ]; then
            echo "[init] $name chroot requires /bin/bash"
            cleanup_chroot_test_root
            return 1
        fi
        chroot_shell=/bin/bash
    elif [ ! -x /mnt/bin/sh ]; then
        echo "[init] $name chroot requires /bin/sh"
        cleanup_chroot_test_root
        return 1
    fi

    echo "[init] run $name in /mnt chroot"
    LD_LIBRARY_PATH= chroot /mnt "$chroot_shell" -c '
        PATH=/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/sbin:/usr/sbin
        HOME=/root
        unset LD_LIBRARY_PATH
        PROFILE_MODE="$7"
        PROFILE_PRESET="$8"
        PROFILE_SAMPLE_HZ="$9"
        export PATH HOME PROFILE_MODE PROFILE_PRESET PROFILE_SAMPLE_HZ
        cd "$1" || exit 1
        if [ -n "$4" ]; then
            if [ -n "$6" ]; then exec "$2" "$3" "$4" "$5" "$6"; fi
            exec "$2" "$3" "$4" "$5"
        fi
        exec "$2" "$3"
    ' sh "$inside_dir" "$chroot_shell" "$inside_runner" \
        "$runner_arg1" "$runner_arg2" "$runner_arg3" \
        "$PROFILE_MODE" "$PROFILE_PRESET" "$PROFILE_SAMPLE_HZ"
    status=$?

    [ -n "$temporary_runner" ] && rm -f "$temporary_runner"
    [ -n "$temporary_rules" ] && rm -f "$temporary_rules"
    cleanup_chroot_test_root
    echo "[init] $name chroot test exit=$status"
    return "$status"
}

run_direct_test_script() {
    script="$1"
    name="${script##*/}"
    case_id="${name%_testcode.sh}"
    phase_rules=""

    if [ -n "$PROFILE_WORKLOAD" ] && [ "$name" = "$PROFILE_WORKLOAD" ] && \
        [ -x /bin/profile-workload-guest ]; then
        if [ -r /etc/mygo-profile-phases ]; then
            phase_rules=/etc/mygo-profile-phases
        fi
        if [ -n "$phase_rules" ]; then
            PROFILE_OUTPUT_ROOT=/mnt/work/mygo-profile \
            PROFILE_MODE="$PROFILE_MODE" PROFILE_PRESET="$PROFILE_PRESET" \
            PROFILE_SAMPLE_HZ="$PROFILE_SAMPLE_HZ" \
                /bin/profile-workload-guest "$case_id" "$script" "$phase_rules"
        else
            PROFILE_OUTPUT_ROOT=/mnt/work/mygo-profile \
            PROFILE_MODE="$PROFILE_MODE" PROFILE_PRESET="$PROFILE_PRESET" \
            PROFILE_SAMPLE_HZ="$PROFILE_SAMPLE_HZ" \
                /bin/profile-workload-guest "$case_id" "$script"
        fi
        return $?
    fi
    /bin/sh "$script"
}

has_ltp_tests() {
    test_dir="$1"

    [ -d "$test_dir/ltp/testcases/bin" ] && [ -d "$test_dir/ltp/runtest" ]
}

install_ltp_testcode_script() {
    test_dir="$1"
    src="/etc/ltp_testcode.sh"
    dst="$test_dir/ltp_testcode.sh"

    [ -f "$src" ] || return 0
    [ -d "$test_dir" ] || return 0
    cp "$src" "$dst" 2>/dev/null || {
        echo "[judge] failed to install legacy LTP entrypoint"
        return 0
    }
    chmod +x "$dst" 2>/dev/null || true
    echo "[judge] installed $dst"
    return 0
}

run_test_script_file() {
    script="$1"
    name="${script##*/}"
    dir="${script%/*}"
    libc_name="${dir##*/}"
    oldpwd="$PWD"
    status=0

    if [ "$TEST_MODE" = "single" ] && [ "$name" != "$TEST_WORKLOAD" ]; then
        echo "[init] skip $libc_name/$name (single workload mode)"
        return 0
    fi

    case "$name" in
        buildstorm_testcode.sh)
            if [ "$BUILDSTORM_ENABLED" != "1" ]; then
                echo "[init] skip $libc_name/$name (temporarily disabled)"
                return 0
            fi
            if [ "$libc_name" != "glibc" ]; then
                echo "[init] skip $libc_name/$name (requires glibc chroot)"
                return 0
            fi
            ;;
        cagent_testcode.sh)
            if [ "$libc_name" != "glibc" ]; then
                echo "[init] skip $libc_name/$name (requires glibc chroot)"
                return 0
            fi
            ;;
    esac

    if _is_blacklisted "$libc_name" "$name"; then
        echo "[init] skip $libc_name/$name (blacklist)"
    else
        if [ "$name" = "basic_testcode.sh" ]; then
            setup_basic_mount_loop
        fi
        echo "[init] run $name"
        case "$name" in
            basic_testcode.sh)
                run_basic_test_script "$script"
                ;;
            busybox_testcode.sh)
                run_busybox_test_script "$script"
                ;;
            libctest_testcode.sh)
                prepare_libctest_dso_links "$dir"
                cd "$dir" || return 0
                /bin/sh "$script"
                ;;
            iozone_testcode.sh|lmbench_testcode.sh)
                run_tmpfs_test_script "$script"
                ;;
            cagent_testcode.sh|buildstorm_testcode.sh)
                run_chroot_test_script "$script"
                ;;
            *)
                cd "$dir" || return 0
                run_direct_test_script "$script"
                ;;
        esac
        status=$?
        cd "$oldpwd" || true
    fi
    if [ "$name" = "busybox_testcode.sh" ]; then
        cleanup_basic_mount_loop
    fi
    return "$status"
}

run_test_scripts() {
    ordered="iozone_testcode.sh lmbench_testcode.sh libcbench_testcode.sh basic_testcode.sh busybox_testcode.sh lua_testcode.sh"
    dirs="/mnt/glibc /mnt/musl"
    test_status=0

    _test_setup
    for dir in $dirs; do
        set -- "$dir"/*_testcode.sh
        if [ -f "$1" ]; then
            echo "[init] found test scripts in $dir"
        fi
    done

    for name in $ordered; do
        for dir in $dirs; do
            script="$dir/$name"
            if [ -f "$script" ] && ! run_test_script_file "$script"; then
                test_status=1
            fi
        done
    done

    for dir in $dirs; do
        set -- "$dir"/*_testcode.sh
        if [ -f "$1" ]; then
            for script in "$dir"/*_testcode.sh; do
                [ -f "$script" ] || continue
                name="${script##*/}"
                case " $ordered ltp_testcode.sh unixbench_testcode.sh " in
                    *" $name "*) continue ;;
                esac
                if ! run_test_script_file "$script"; then
                    test_status=1
                fi
            done
        fi
    done

    for dir in $dirs; do
        if has_ltp_tests "$dir"; then
            install_ltp_testcode_script "$dir"
            script="$dir/ltp_testcode.sh"
            if [ -f "$script" ] && ! run_test_script_file "$script"; then
                test_status=1
            fi
        fi
    done

    cleanup_basic_mount_loop
    _test_teardown
    return "$test_status"
}

setup_dynamic_linker() {
    glibc_lib=/mnt/glibc/lib

    if [ ! -e /lib ] && [ ! -L /lib ]; then
        ln -s "$glibc_lib" /lib 2>/dev/null || true
    fi
    if [ ! -e /lib64 ] && [ ! -L /lib64 ]; then
        ln -s "$glibc_lib" /lib64 2>/dev/null || true
    fi

    case ":${LD_LIBRARY_PATH:-}:" in
        *:"$glibc_lib":*) ;;
        *) LD_LIBRARY_PATH="$glibc_lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" ;;
    esac
    export LD_LIBRARY_PATH

    echo "[init] dynamic linker path prepared"
}

shutdown_after_tests() {
    sync || true
    poweroff -f >/dev/null 2>&1 || poweroff >/dev/null 2>&1 || reboot -f >/dev/null 2>&1 || true
}

mount -t ext4 /dev/vd0 /mnt 2>/dev/null || true
maybe_enter_shell
setup_dynamic_linker
echo "[init] mount testdisk OK (or skipped)"
echo "[init] ls /mnt:"
/bin/ls /mnt/ 2>&1 || true
echo "[init] ls done"

run_test_scripts
test_status=$?
echo "[init] test scripts finished with status $test_status, shutting down"
shutdown_after_tests

echo "[init] shutdown failed, entering shell"
exec /bin/sh -i
