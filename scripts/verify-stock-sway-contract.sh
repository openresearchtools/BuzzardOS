#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

source_dir=${1:?usage: verify-stock-sway-contract.sh SWAY_SOURCE_DIR}
expected_commit=88869399f421d9180dd8b6ed0b5a1f4a3585d252

fail() {
    echo "stock Sway contract check failed: $*" >&2
    exit 1
}

require_source() {
    local relative=$1
    local literal=$2
    grep -Fq -- "$literal" "$source_dir/$relative" ||
        fail "$relative no longer contains: $literal"
}

[[ -d "$source_dir/.git" ]] || fail "$source_dir is not a Sway Git checkout"
[[ $(git -C "$source_dir" rev-parse HEAD) == "$expected_commit" ]] ||
    fail "Sway checkout is not pinned to $expected_commit"
git -C "$source_dir" diff --quiet -- ||
    fail "tracked Sway source differs from the pinned commit"
git -C "$source_dir" diff --cached --quiet -- ||
    fail "the pinned Sway checkout has staged source changes"

# Stock Sway renders one server-owned titlebar scene in the same container
# tree as the content and supplies titlebar dragging without a shell overlay.
require_source include/sway/tree/container.h \
    "struct sway_text_node *title_text;"
require_source include/sway/tree/container.h \
    "struct sway_text_node *marks_text;"
require_source sway/tree/container.c \
    "void container_arrange_title_bar(struct sway_container *con)"
require_source sway/input/seatop_default.c \
    "bool titlebar_left_btn_pressed = on_titlebar && button == BTN_LEFT;"
require_source sway/input/seatop_default.c \
    "seatop_begin_move_floating(seat, container_toplevel_ancestor(cont));"

# find_edge independently accumulates the four edge bits. Combined bits reach
# the same floating resize seat operation, which is the stock corner path.
require_source sway/input/seatop_default.c \
    "cursor->cursor->x < cont->pending.x + cont->pending.border_thickness"
require_source sway/input/seatop_default.c \
    "cursor->cursor->y < cont->pending.y + cont->pending.border_thickness"
require_source sway/input/seatop_default.c \
    "cursor->cursor->x >= cont->pending.x + cont->pending.width - cont->pending.border_thickness"
require_source sway/input/seatop_default.c \
    "cursor->cursor->y >= cont->pending.y + cont->pending.height - cont->pending.border_thickness"
require_source sway/input/seatop_default.c \
    "seatop_begin_resize_floating(seat, cont, resize_edge);"
require_source sway/input/seatop_resize_floating.c \
    "if (edge & WLR_EDGE_LEFT)"
require_source sway/input/seatop_resize_floating.c \
    "if (edge & WLR_EDGE_TOP)"

# Close and the state/geometry operations remain stock compositor commands.
# They are IPC/input actions, not titlebar button scenes in this Sway release.
require_source sway/commands/kill.c "view_close(con->view);"
require_source sway/commands.c '{ "fullscreen", cmd_fullscreen },'
require_source sway/commands.c '{ "scratchpad", cmd_scratchpad },'

if grep -Eiq \
    'title.?bar.*(minimi[sz]e|maximi[sz]e|close)|(minimi[sz]e|maximi[sz]e|close).*title.?bar|TITLEBAR_BUTTON' \
    "$source_dir/include/sway/tree/container.h" \
    "$source_dir/sway/tree/container.c" \
    "$source_dir/sway/input/seatop_default.c"; then
    fail "pinned stock Sway unexpectedly contains titlebar window-button code"
fi

echo "Verified stock Sway $expected_commit titlebar drag/resize contract and button limitation"
