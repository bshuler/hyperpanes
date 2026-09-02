# Hyperpanes — a Linux box the GUI can be driven inside, with nobody watching.
#
# The point is autonomy: every input test I could otherwise only ask a human to
# perform (type this, press that, drag this file onto that pane) happens in here
# against a real window on a real X server, with synthetic events that cannot
# touch anybody's actual desktop or steal anybody's focus.
#
# Rendering: the app links Slint's femtovg-wgpu renderer (wgpu 29), so a stub X
# server is not enough on its own — it needs a Vulkan device. mesa's lavapipe
# (`libvulkan1` + `mesa-vulkan-drivers`) is a software Vulkan ICD, which is
# exactly that: slow, headless, and completely deterministic.
FROM rust:1-bookworm

# Build deps mirror .github/workflows/test.yml's Linux leg, so a green harness
# and a green CI leg mean the same thing.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libfontconfig1-dev libfreetype6-dev libxkbcommon-dev libwayland-dev \
        libxcb-shape0-dev libxcb-xfixes0-dev libssl-dev pkg-config cmake clang \
        libasound2-dev \
    # runtime: X11 + software Vulkan + fonts
    && apt-get install -y --no-install-recommends \
        libxkbcommon-x11-0 libx11-6 libxcb1 libgl1 libegl1 \
        libvulkan1 mesa-vulkan-drivers vulkan-tools \
        fonts-dejavu-core fontconfig \
    # the input rig: a display to draw on, and hands to drive it
    && apt-get install -y --no-install-recommends \
        xvfb x11-utils x11-xserver-utils xdotool xclip imagemagick \
    # A window manager, which is not optional. With a bare X server nothing maps
    # or focuses a new window and `xdotool windowactivate` has no EWMH to talk
    # to, so focus lands wherever it likes and synthetic keys are delivered to a
    # window that is not listening. openbox is the smallest thing that makes
    # focus behave the way it does on a real desktop.
        openbox \
    # python3-xlib is the XDND source: Debian has no `dragon-drop` in bookworm,
    # and a scripted drag beats a GUI drag tool anyway — the harness needs to
    # choose the offered mime types, not accept whatever a helper app sends.
        python3-xlib procps psmisc jq \
    && rm -rf /var/lib/apt/lists/*

# Software Vulkan, and no GPU probing that can hang on a machine with no GPU.
# XDG_RUNTIME_DIR is unset in a container, which makes every Vulkan/Wayland
# layer print a startup error before doing the right thing anyway. Pointing it
# at a real directory keeps the harness log readable.
ENV XDG_RUNTIME_DIR=/tmp/runtime \
    VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
    WGPU_BACKEND=vulkan \
    LIBGL_ALWAYS_SOFTWARE=1 \
    DISPLAY=:99 \
    CARGO_TERM_COLOR=never

RUN mkdir -p /tmp/runtime && chmod 700 /tmp/runtime

WORKDIR /work
