// SPDX-License-Identifier: AGPL-3.0-or-later
// Minimal acceptance probe for the private guest Wayland registry.

#include <stdio.h>
#include <string.h>
#include <wayland-client.h>

struct registry_state {
    int fractional_scale;
    int viewporter;
};

static void global(void *data, struct wl_registry *registry, uint32_t name,
                   const char *interface, uint32_t version) {
    struct registry_state *state = data;
    (void)registry;
    (void)name;
    printf("%s %u\n", interface, version);
    if (strcmp(interface, "wp_fractional_scale_manager_v1") == 0) {
        state->fractional_scale = 1;
    } else if (strcmp(interface, "wp_viewporter") == 0) {
        state->viewporter = 1;
    }
}

static void global_remove(void *data, struct wl_registry *registry,
                          uint32_t name) {
    (void)data;
    (void)registry;
    (void)name;
}

static const struct wl_registry_listener listener = {
    .global = global,
    .global_remove = global_remove,
};

int main(void) {
    struct registry_state state = {0};
    struct wl_display *display = wl_display_connect(NULL);
    if (display == NULL) {
        fputs("failed to connect to the guest Wayland display\n", stderr);
        return 2;
    }
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &listener, &state);
    if (wl_display_roundtrip(display) < 0) {
        fputs("failed to read the guest Wayland registry\n", stderr);
        return 2;
    }
    wl_registry_destroy(registry);
    wl_display_disconnect(display);
    if (!state.fractional_scale || !state.viewporter) {
        fprintf(stderr,
                "required scaling globals: fractional-scale=%s viewporter=%s\n",
                state.fractional_scale ? "yes" : "no",
                state.viewporter ? "yes" : "no");
        return 1;
    }
    return 0;
}
