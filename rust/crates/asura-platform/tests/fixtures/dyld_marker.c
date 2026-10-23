/* PC-I38 fixture (person-command-mode.md). When a process loads this
 * library, its constructor writes MARKER_DIR/loaded.<pid>. MARKER_DIR is a
 * test scratch directory fixed at compile time; the library reads no
 * environment and writes nowhere else. */
#include <stdio.h>
#include <unistd.h>

__attribute__((constructor)) static void asura_dyld_marker(void) {
    char path[1024];
    if (snprintf(path, sizeof path, "%s/loaded.%d", MARKER_DIR, (int)getpid()) >= (int)sizeof path) {
        return;
    }
    FILE *file = fopen(path, "w");
    if (file != NULL) {
        fputs("loaded\n", file);
        fclose(file);
    }
}
