/* Real xdg-toplevel parents, transient dialogs and client pointer requests. */
#include <gtk/gtk.h>
#include <glib-unix.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static GtkWidget *parent, *dialog;
static const char *main_role;
static int gesture;

static gboolean draw(GtkWidget *widget, cairo_t *cr, gpointer data) {
    const char *role = data;
    if (!strcmp(role, "dialog")) cairo_set_source_rgb(cr, 0.1, 0.9, 0.1);
    else if (!strcmp(role, "second")) cairo_set_source_rgb(cr, 0.1, 0.1, 0.9);
    else cairo_set_source_rgb(cr, 0.9, 0.1, 0.1);
    cairo_paint(cr);
    return TRUE;
}

static gboolean configure(GtkWidget *widget, GdkEventConfigure *event, gpointer data) {
    printf("{\"kind\":\"configure\",\"role\":\"%s\",\"width\":%d,\"height\":%d}\n",
           (char *)data, event->width, event->height);
    fflush(stdout);
    return FALSE;
}

static gboolean key(GtkWidget *widget, GdkEventKey *event, gpointer data) {
    printf("{\"kind\":\"key\",\"role\":\"%s\",\"keyval\":%u,\"state\":%u}\n",
           (char *)data, event->keyval, event->state);
    fflush(stdout);
    return TRUE;
}

static gboolean press(GtkWidget *widget, GdkEventButton *event, gpointer data) {
    printf("{\"kind\":\"button\",\"role\":\"%s\",\"button\":%u,\"pressed\":true,\"event_type\":%u}\n",
           (char *)data, event->button, event->type);
    fflush(stdout);
    if (gesture == 1) {
        gtk_window_begin_move_drag(GTK_WINDOW(widget), event->button,
                                   event->x_root, event->y_root, event->time);
    } else if (gesture == 2) {
        gtk_window_begin_resize_drag(GTK_WINDOW(widget), GDK_WINDOW_EDGE_NORTH_WEST,
                                     event->button, event->x_root, event->y_root, event->time);
    }
    printf("{\"kind\":\"gesture\",\"role\":\"%s\",\"operation\":%d}\n",
           (char *)data, gesture);
    fflush(stdout);
    gesture = 0;
    return TRUE;
}

static gboolean release(GtkWidget *widget, GdkEventButton *event, gpointer data) {
    printf("{\"kind\":\"button\",\"role\":\"%s\",\"button\":%u,\"pressed\":false,\"event_type\":%u}\n",
           (char *)data, event->button, event->type);
    fflush(stdout);
    return TRUE;
}

static void destroyed(GtkWidget *widget, gpointer data) {
    printf("{\"kind\":\"closed\",\"role\":\"%s\"}\n", (char *)data);
    fflush(stdout);
    if (widget == parent) gtk_main_quit();
    if (widget == dialog) dialog = NULL;
}

static GtkWidget *window(const char *role, GtkWidget *transient) {
    GtkWidget *widget = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    char *title = g_strdup_printf("Reka fixture %s", role);
    gtk_window_set_title(GTK_WINDOW(widget), title);
    g_free(title);
    gtk_window_set_decorated(GTK_WINDOW(widget), FALSE);
    gtk_window_set_default_size(GTK_WINDOW(widget), 320, 240);
    if (transient) {
        gtk_window_set_transient_for(GTK_WINDOW(widget), GTK_WINDOW(transient));
        gtk_window_set_destroy_with_parent(GTK_WINDOW(widget), TRUE);
        GdkGeometry hints = {.min_width=160, .min_height=120, .max_width=500, .max_height=360};
        gtk_window_set_geometry_hints(GTK_WINDOW(widget), NULL, &hints,
                                     GDK_HINT_MIN_SIZE | GDK_HINT_MAX_SIZE);
    }
    gtk_widget_add_events(widget, GDK_BUTTON_PRESS_MASK | GDK_BUTTON_RELEASE_MASK | GDK_KEY_PRESS_MASK);
    g_signal_connect(widget, "draw", G_CALLBACK(draw), (gpointer)role);
    g_signal_connect(widget, "configure-event", G_CALLBACK(configure), (gpointer)role);
    g_signal_connect(widget, "key-press-event", G_CALLBACK(key), (gpointer)role);
    g_signal_connect(widget, "button-press-event", G_CALLBACK(press), (gpointer)role);
    g_signal_connect(widget, "button-release-event", G_CALLBACK(release), (gpointer)role);
    g_signal_connect(widget, "destroy", G_CALLBACK(destroyed), (gpointer)role);
    gtk_widget_show_all(widget);
    return widget;
}

static gboolean command(gint fd, GIOCondition condition, gpointer data) {
    char line[128];
    if (!fgets(line, sizeof(line), stdin)) { gtk_main_quit(); return G_SOURCE_REMOVE; }
    g_strchomp(line);
    if (!strcmp(line, "dialog") && !dialog) dialog = window("dialog", parent);
    else if (!strcmp(line, "client-move")) gesture = 1;
    else if (!strcmp(line, "client-resize")) gesture = 2;
    else if (!strcmp(line, "close-dialog") && dialog) gtk_widget_destroy(dialog);
    else if (!strcmp(line, "close-parent")) gtk_widget_destroy(parent);
    else if (!strcmp(line, "quit")) gtk_main_quit();
    return G_SOURCE_CONTINUE;
}

int main(int argc, char **argv) {
    main_role = argc > 1 ? argv[1] : "parent";
    g_set_prgname("reka-floating-fixture");
    gtk_init(&argc, &argv);
    parent = window(main_role, NULL);
    g_unix_fd_add(STDIN_FILENO, G_IO_IN | G_IO_HUP, command, NULL);
    gtk_main();
    return 0;
}
