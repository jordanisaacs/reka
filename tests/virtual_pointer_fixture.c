/* Synthetic pointer events are sent only through this private Wayland socket. */
#include <wayland-client.h>
#include "virtual-pointer-client.h"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static struct zwlr_virtual_pointer_manager_v1 *manager;
static struct wl_seat *seat;
static void capabilities(void *data, struct wl_seat *seat, uint32_t value) {}
static const struct wl_seat_listener seat_listener = {.capabilities=capabilities};

static void global(void *data, struct wl_registry *registry, uint32_t name,
                   const char *interface, uint32_t version) {
    if (!strcmp(interface, "zwlr_virtual_pointer_manager_v1"))
        manager = wl_registry_bind(registry, name, &zwlr_virtual_pointer_manager_v1_interface, 1);
    else if (!strcmp(interface, "wl_seat") && !seat) {
        seat = wl_registry_bind(registry, name, &wl_seat_interface, 1);
        wl_seat_add_listener(seat, &seat_listener, NULL);
    }
}

static void removed(void *data, struct wl_registry *registry, uint32_t name) {}
static const struct wl_registry_listener listener = {.global=global, .global_remove=removed};

static uint32_t milliseconds(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint32_t)(now.tv_sec * 1000 + now.tv_nsec / 1000000);
}

int main(void) {
    const char *runtime = getenv("XDG_RUNTIME_DIR");
    if (!runtime || !strstr(runtime, "reka-floating-")) {
        fprintf(stderr, "Refusing pointer injection outside private integration runtime\n");
        return 1;
    }
    struct wl_display *display = wl_display_connect(NULL);
    if (!display) return 2;
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &listener, NULL);
    if (wl_display_roundtrip(display) < 0 || !manager || !seat) return 3;
    struct zwlr_virtual_pointer_v1 *pointer =
        zwlr_virtual_pointer_manager_v1_create_virtual_pointer(manager, seat);
    char line[128], operation[32];
    int x, y;
    while (fgets(line, sizeof(line), stdin)) {
        if (sscanf(line, "%31s %d %d", operation, &x, &y) != 3) return 4;
        if (!strcmp(operation, "absolute"))
            zwlr_virtual_pointer_v1_motion_absolute(pointer, milliseconds(), x, y, 1280, 720);
        else if (!strcmp(operation, "motion"))
            zwlr_virtual_pointer_v1_motion(pointer, milliseconds(), wl_fixed_from_int(x), wl_fixed_from_int(y));
        else if (!strcmp(operation, "button"))
            zwlr_virtual_pointer_v1_button(pointer, milliseconds(), x, y);
        else return 5;
        zwlr_virtual_pointer_v1_frame(pointer);
        if (wl_display_roundtrip(display) < 0) return 6;
        puts("OK");
        fflush(stdout);
    }
    zwlr_virtual_pointer_v1_destroy(pointer);
    wl_display_roundtrip(display);
    wl_display_disconnect(display);
    return 0;
}
