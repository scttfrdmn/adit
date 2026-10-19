#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Bootstrap an adit box: EFA, NIXL (pinned, LIBFABRIC plugin), nixlbench and its etcd client,
# Rust, and the adit workspace built against the real NIXL (never the stub).
#
#   sudo bash hack/install.sh proxy    # GPU-less EFA proxy (Ubuntu 24.04 arm64)
#   sudo bash hack/install.sh client   # GPU client (DLAMI base: driver, CUDA, EFA preinstalled)
#
# Idempotent: each stage leaves a marker in /var/lib/adit and is skipped next time, so the same
# script bakes an AMI and refreshes a box booted from one. Versions are pinned here and echoed to
# /var/lib/adit/versions for the capability matrix.
set +e
set -uo pipefail
echo "opts=$-"

ROLE=${1-}
[[ $ROLE == proxy || $ROLE == client ]] || { echo "usage: $0 proxy|client" >&2; exit 2; }
NIXL_TAG=${NIXL_TAG:-v1.2.0}
# v0.15.4 (latest tag) uses GPR_ASSERT, which gRPC 1.73 removed; NIXL builds master. Pinned here.
ETCD_CPP_REF=${ETCD_CPP_REF:-7c6e714f188f9576e25e0350cac4181139eec23e}
# NIXL needs absl/log, newer than Ubuntu 24.04's Abseil; gRPC (for etcd-cpp, for nixlbench) must
# then be built against the same Abseil, or two Abseils meet at runtime. Same recipe and tags as
# NIXL's own contrib/Dockerfile at the pinned tag.
ABSL_TAG=${ABSL_TAG:-lts_2025_08_14}
GRPC_TAG=${GRPC_TAG:-v1.73.0}
NIXL_PREFIX=${NIXL_PREFIX:-/opt/nvidia/nvda_nixl}
ADIT_REPO=${ADIT_REPO:-https://github.com/scttfrdmn/adit.git}
ADIT_REF=${ADIT_REF:-main}
ADIT_USER=${ADIT_USER:-ubuntu}
SRC=/opt/adit-src
STATE=/var/lib/adit
mkdir -p "$STATE" "$SRC"
LOG=$STATE/install.log
exec > >(tee -a "$LOG") 2>&1

log() { echo "[$(date -u +%FT%TZ)] $*"; }
done_() { [[ -f $STATE/$1.done ]]; }
mark() { touch "$STATE/$1.done"; }
fail() { log "FAILED: $*"; exit 1; }

arch=$(uname -m)
log "role=$ROLE arch=$arch nixl=$NIXL_TAG prefix=$NIXL_PREFIX"

if ! done_ apt; then
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -y || fail apt-get update
    apt-get install -y build-essential cmake meson ninja-build pkg-config git curl jq unzip \
        libhwloc-dev hwloc libnuma-dev libaio-dev libssl-dev libcurl4-openssl-dev \
        python3-pip python3-venv pybind11-dev libgflags-dev libgtest-dev clang libclang-dev \
        libprotobuf-dev libcpprest-dev etcd-server etcd-client autotools-dev automake libtool zlib1g-dev uuid-dev \
        linux-tools-common "linux-tools-$(uname -r)" numactl rdma-core ibverbs-utils \
        || fail apt-get install
    mark apt
fi

if ! done_ awscli; then
    if ! command -v aws >/dev/null; then
        a=$([[ $arch == aarch64 ]] && echo aarch64 || echo x86_64)
        curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-$a.zip" -o /tmp/awscli.zip && unzip -q -o /tmp/awscli.zip -d /tmp && /tmp/aws/install --update || fail awscli
    fi
    mark awscli
fi

if ! done_ efa; then
    if [[ -x /opt/amazon/efa/bin/fi_info ]]; then
        log "EFA already installed (DLAMI)"
    else
        cd /tmp && curl -fsSL https://efa-installer.amazonaws.com/aws-efa-installer-latest.tar.gz | tar xz || fail efa download
        cd aws-efa-installer || fail efa dir
        # The Ubuntu AWS kernel ships the efa module; build it only if it is missing.
        kmod=""
        modinfo efa >/dev/null 2>&1 && kmod="--skip-kmod"
        ./efa_installer.sh -y $kmod --skip-limit-conf || fail efa_installer
    fi
    mark efa
fi
export PATH=/opt/amazon/efa/bin:$PATH

# The EFA installer's own limits file is skipped (--skip-limit-conf), which left memlock at the
# distro default (~15 GiB on c8gn) and refused registrations of >= 16 GiB (#19). Pinned memory
# for NIXL registration is unbounded by design; new login sessions pick this up.
if ! done_ memlock; then
    echo '* - memlock unlimited' >/etc/security/limits.d/90-adit-memlock.conf
    mark memlock
fi

CUDA=""
if [[ $ROLE == client ]]; then
    CUDA=$(ls -d /usr/local/cuda 2>/dev/null)
    [[ -n $CUDA ]] || fail "no /usr/local/cuda on a client box"
    # sudo resets PATH; NIXL's meson needs nvcc for its CUDA kernels.
    export PATH=$CUDA/bin:$PATH
fi

if ! done_ absl; then
    # A distro Abseil left installed makes NIXL's meson refuse (version mix); remove it first.
    apt-get remove -y --purge libabsl-dev libgrpc++-dev protobuf-compiler-grpc 2>/dev/null
    cd "$SRC" && rm -rf abseil-cpp && git clone -q --depth 1 -b "$ABSL_TAG" https://github.com/abseil/abseil-cpp.git || fail absl clone
    cmake -S abseil-cpp -B abseil-cpp/build -G Ninja -DCMAKE_INSTALL_PREFIX=/usr/local -DCMAKE_BUILD_TYPE=Release \
        -DBUILD_SHARED_LIBS=ON -DABSL_PROPAGATE_CXX_STD=ON -DABSL_ENABLE_INSTALL=ON && ninja -C abseil-cpp/build install || fail absl build
    ldconfig
    mark absl
fi

if ! done_ grpc; then
    cd "$SRC" && rm -rf grpc && git clone -q --recurse-submodules -b "$GRPC_TAG" --depth 1 --shallow-submodules https://github.com/grpc/grpc || fail grpc clone
    cmake -S grpc -B grpc/cmake/build -G Ninja -DgRPC_INSTALL=ON -DgRPC_BUILD_TESTS=OFF -DBUILD_SHARED_LIBS=ON \
        -DCMAKE_CXX_STANDARD=17 -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local -DCMAKE_PREFIX_PATH=/usr/local \
        -Dabsl_DIR=/usr/local/lib/cmake/absl -DgRPC_SSL_PROVIDER=package -DgRPC_ABSL_PROVIDER=package \
        -DgRPC_PROTOBUF_PROVIDER=module -DgRPC_ZLIB_PROVIDER=package && ninja -C grpc/cmake/build install || fail grpc build
    ldconfig
    mark grpc
fi

if ! done_ etcdcpp; then
    cd "$SRC" && rm -rf etcd-cpp-apiv3 && git clone -q https://github.com/etcd-cpp-apiv3/etcd-cpp-apiv3.git || fail etcd-cpp clone
    cd etcd-cpp-apiv3 && git checkout -q "$ETCD_CPP_REF" || fail etcd-cpp checkout
    sed -i '/^find_dependency(cpprestsdk)$/d' etcd-cpp-api-config.in.cmake
    cmake -B build -G Ninja -DBUILD_ETCD_CORE_ONLY=ON -DCMAKE_BUILD_TYPE=Release -DETCD_CMAKE_CXX_STANDARD=17 \
        -DCMAKE_PREFIX_PATH=/usr/local -DBUILD_ETCD_TESTS=OFF && ninja -C build install || fail etcd-cpp build
    ldconfig
    mark etcdcpp
fi

if ! done_ nixl; then
    cd "$SRC" && rm -rf nixl && git clone -q --depth 1 -b "$NIXL_TAG" https://github.com/ai-dynamo/nixl.git || fail nixl clone
    cd nixl
    opts=(-Dlibfabric_path=/opt/amazon/efa -Detcd_inc_path=/usr/local/include -Detcd_lib_path=/usr/local/lib
        -Dbuild_tests=false -Dbuild_examples=false --buildtype=release --prefix="$NIXL_PREFIX")
    [[ -n $CUDA ]] && opts+=(-Dcudapath_inc="$CUDA/include" -Dcudapath_lib="$CUDA/lib64")
    meson setup build "${opts[@]}" || fail nixl meson
    ninja -C build install || fail nixl build
    echo "$NIXL_PREFIX/lib/$arch-linux-gnu" >/etc/ld.so.conf.d/nixl.conf
    echo "$NIXL_PREFIX/lib/$arch-linux-gnu/plugins" >>/etc/ld.so.conf.d/nixl.conf
    echo "$NIXL_PREFIX/lib" >>/etc/ld.so.conf.d/nixl.conf
    ldconfig
    cd benchmark/nixlbench || fail nixlbench dir
    bopts=(-Dnixl_path="$NIXL_PREFIX" -Detcd_inc_path=/usr/local/include -Detcd_lib_path=/usr/local/lib --prefix="$NIXL_PREFIX")
    [[ -n $CUDA ]] && bopts+=(-Dcudapath_inc="$CUDA/include" -Dcudapath_lib="$CUDA/lib64")
    meson setup build "${bopts[@]}" || fail nixlbench meson
    ninja -C build install || fail nixlbench build
    ln -sf "$NIXL_PREFIX/bin/nixlbench" /usr/local/bin/nixlbench
    mark nixl
fi
plugins=$(find "$NIXL_PREFIX" -name 'libplugin_LIBFABRIC*.so' | head -1)
[[ -n $plugins ]] || fail "NIXL LIBFABRIC plugin not built"

if ! done_ rust; then
    sudo -u "$ADIT_USER" -H bash -c 'curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal' || fail rustup
    mark rust
fi

if ! done_ bgrun; then
    cat >/usr/local/bin/bgrun <<'B'
#!/usr/bin/env bash
# bgrun [-o OUT] CMD...: run CMD detached, output to OUT (default /tmp/bgrun.out); poll with tail.
out=/tmp/bgrun.out
if [[ ${1-} == -o ]]; then out=$2; shift 2; fi
: >"$out"
nohup "$@" >>"$out" 2>&1 &
echo "bgrun pid=$! out=$out"
B
    chmod +x /usr/local/bin/bgrun
    mark bgrun
fi

# Path A and B tools on clients: s5cmd, and a Python env with CuPy, KvikIO, the S3 Connector for
# PyTorch, PyTorch and adit-py (the shared GPU oracle). Package names track the CUDA major version.
if [[ $ROLE == client ]] && ! done_ pathtools; then
    if ! command -v s5cmd >/dev/null; then
        v=2.3.0
        curl -fsSL "https://github.com/peak/s5cmd/releases/download/v$v/s5cmd_${v}_Linux-64bit.tar.gz" | tar xz -C /usr/local/bin s5cmd || fail s5cmd
    fi
    python3 -m venv /opt/adit-venv || fail venv
    V=/opt/adit-venv/bin/pip
    $V install -q --upgrade pip maturin patchelf || fail pip
    cu=$(nvcc --version 2>/dev/null | sed -n 's/.*release \([0-9]*\)\..*/\1/p')
    $V install -q "cupy-cuda${cu}x" || $V install -q cupy-cuda12x || fail cupy
    $V install -q "kvikio-cu${cu}" || $V install -q kvikio-cu12 || fail kvikio
    $V install -q torch --index-url "https://download.pytorch.org/whl/cu${cu}0" || $V install -q torch || fail torch
    $V install -q s3torchconnector || fail s3torchconnector
    chown -R "$ADIT_USER" /opt/adit-venv
    mark pathtools
fi

# The adit workspace: always refreshed (code changes between runs; the toolchain does not).
log "building adit $ADIT_REF"
if [[ ! -d $SRC/adit/.git ]]; then
    git clone -q "$ADIT_REPO" "$SRC/adit" || fail adit clone
fi
chown -R "$ADIT_USER" "$SRC/adit"
sudo -u "$ADIT_USER" -H bash -c "cd $SRC/adit && git fetch -q origin && git checkout -q $ADIT_REF && git reset -q --hard origin/$ADIT_REF 2>/dev/null || git reset -q --hard $ADIT_REF" || fail adit checkout
pkgs="-p adit-proxy"
[[ $ROLE == client ]] && pkgs="-p adit-client -p adit-proxy"
sudo -u "$ADIT_USER" -H bash -c "cd $SRC/adit && NIXL_PREFIX=$NIXL_PREFIX NIXL_NO_STUBS_FALLBACK=1 ~/.cargo/bin/cargo build --release $pkgs" || fail "adit build (real NIXL)"
if [[ $ROLE == client ]]; then
    # adit-py into the path-tools venv, against the real NIXL.
    rm -rf /tmp/adit-wheels
    sudo -u "$ADIT_USER" -H bash -c "cd $SRC/adit/crates/adit-py && NIXL_PREFIX=$NIXL_PREFIX NIXL_NO_STUBS_FALLBACK=1 PATH=\$HOME/.cargo/bin:\$PATH /opt/adit-venv/bin/maturin build -q --release --auditwheel skip -o /tmp/adit-wheels" || fail "adit-py build"
    /opt/adit-venv/bin/pip install -q --force-reinstall /tmp/adit-wheels/adit-*.whl || fail "adit-py install"
fi
for b in adit-proxy adit-nixl-probe-init adit-get adit-load adit-nixl-probe-target adit-fetch-bench; do
    [[ -x $SRC/adit/target/release/$b ]] && ln -sf "$SRC/adit/target/release/$b" /usr/local/bin/$b
done

{
    echo "role=$ROLE"
    echo "arch=$arch"
    echo "kernel=$(uname -r)"
    echo "nixl=$NIXL_TAG"
    echo "etcd_cpp=$ETCD_CPP_REF"
    echo "absl=$ABSL_TAG"
    echo "grpc=$GRPC_TAG"
    echo "efa_installer=$(cat /opt/amazon/efa_installed_packages 2>/dev/null | grep -m1 -o 'EFA installer version: [0-9.]*' | awk '{print $4}')"
    echo "libfabric=$(fi_info --version 2>/dev/null | awk '/libfabric:/{print $2}')"
    echo "driver=$(nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null | head -1)"
    echo "cuda=$(nvcc --version 2>/dev/null | awk -F'release ' '/release/{print $2}' | cut -d, -f1)"
    echo "adit=$(cd $SRC/adit && git rev-parse --short HEAD)"
    echo "memlock_new_sessions=$(sudo -u "$ADIT_USER" -i bash -c 'ulimit -l')"
} >"$STATE/versions"
cat "$STATE/versions"
log "install complete"
