# reims-os-vgpu: the virtual GPU, its QEMU, and the boot scripts that drive them.
#
# Two install roots, and the split is load-bearing:
#
#   /usr/lib/reims-os-vgpu       the tree, because vm/boot-x86.sh resolves its
#                                own REPO_ROOT from where it sits and reads
#                                vm/ovmf beneath it.
#   /usr/libexec/reims-os-vgpu   the QEMU binary, deliberately NOT at the
#                                in-tree vendor/qemu/build path. boot-x86.sh
#                                rebuilds QEMU when QEMU_BIN equals that
#                                default, which cannot work on a bootc system:
#                                /usr is read-only and the image has no
#                                toolchain. reims-os-installer writes QEMU_BIN
#                                pointing here, which takes the branch that uses
#                                the binary as found.
#
# NOT YET BUILT. This spec has not been through rpmbuild.

Name:           reims-os-vgpu
Version:        0.1.0
Release:        1%{?dist}
Summary:        Virtual GPU and QEMU runtime for running macOS under Reims OS

# Two works in one package: this project is LGPL-3 (see LICENSE), and the QEMU
# binary built into it is GPL-2. The field names both rather than the more
# permissive one, because the binary is what most of the package's bytes are.
License:        LGPL-3.0-only AND GPL-2.0-only
URL:            https://github.com/Project-Reims-Community/reims-os-vgpu
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  make
BuildRequires:  ninja-build
BuildRequires:  meson
BuildRequires:  glib2-devel
BuildRequires:  pixman-devel
BuildRequires:  vulkan-loader-devel
BuildRequires:  vulkan-headers
BuildRequires:  python3

# The guest cannot boot without these at runtime, and each failure is late and
# unhelpful: a missing shader tool surfaces as the guest's first draw failing.
Requires:       vulkan-loader
Requires:       llvm
Requires:       spirv-tools
Requires:       edk2-ovmf

%description
The Reims virtual GPU: a QEMU display device backed by a Vulkan translation
layer, the QEMU build it is linked into, and the scripts that boot a macOS
guest against it.

Installed for reims-os-session to launch. The boot scripts' persistent class is
what an installed system uses: fixed paths, writable, nothing reverted.

%prep
%autosetup -n %{name}-%{version}

%build
# The crate is a staticlib linked into QEMU rather than a shared object, so the
# QEMU build is the only artifact that matters here.
scripts/qemu-build/qemu-build.sh --target x86_64 --backend vulkan

%install
install -d %{buildroot}%{_prefix}/lib/%{name}
cp -a vm %{buildroot}%{_prefix}/lib/%{name}/vm
# Working state belongs in /var; the packaged tree is read-only.
rm -rf %{buildroot}%{_prefix}/lib/%{name}/vm/disks

install -Dpm 0755 vendor/qemu/build/qemu-system-x86_64 \
    %{buildroot}%{_libexecdir}/%{name}/qemu-system-x86_64

%check
# The launcher resolves this path and nothing else; a package whose boot script
# is missing or unexecutable installs cleanly and fails at login.
test -x %{buildroot}%{_prefix}/lib/%{name}/vm/boot-x86.sh
# The persistent class is what an installed system boots. Without it the login
# session would fall back to a class that reverts the guest.
grep -q -- '--persistent' %{buildroot}%{_prefix}/lib/%{name}/vm/boot-x86.sh
%{buildroot}%{_libexecdir}/%{name}/qemu-system-x86_64 --version

%files
%license LICENSE
%doc README.md
%{_prefix}/lib/%{name}/
%{_libexecdir}/%{name}/

%changelog
* Thu Aug 13 2026 Project Reims Community <community@example.invalid> - 0.1.0-1
- Initial package.
