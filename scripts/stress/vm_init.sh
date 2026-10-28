#!/bin/busybox sh
export PATH=/bin
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox ifconfig lo 127.0.0.1 up
/seed-entropy
echo ENTROPY_STATUS "$?"
echo XMTR_VM_BEGIN
/bin/busybox uname -a
/bin/busybox free
for binary in /xmtr32 /xmtr64; do
    echo TEST_VERSION "$binary"
    "$binary" --version
    echo STATUS "$?"
    echo TEST_TRACE "$binary"
    "$binary" --no-upload --no-rdns --quiet --json -p icmp -c 1 -t 2s 127.0.0.1
    echo STATUS "$?"
    echo TEST_HUGE_TIME "$binary"
    "$binary" --no-upload --no-rdns --quiet --json -p icmp -c 30 -t 18446744073709551615s 127.0.0.1
    echo STATUS "$?"
    echo TEST_PARALLEL "$binary"
    set --
    count=0
    while [ "$count" -lt 64 ]; do
        set -- "$@" 127.0.0.1
        count=$((count + 1))
    done
    "$binary" --no-upload --no-rdns --quiet -p icmp -c 1 -t 2s -j 64 "$@"
    echo STATUS "$?"
done
echo XMTR_VM_END
/bin/busybox poweroff -f
