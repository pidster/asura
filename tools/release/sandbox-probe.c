/*
 * SG-V5 negative fixture (release-signing.md, HS-P7). test-asura-bundle
 * --sandbox signs this program with the helper's identifier and entitlement
 * list, so that it runs in the same App Sandbox as the helper, and once
 * without entitlements as the control. It reports one line per attempt:
 *
 *   sandboxed yes|no
 *   ssh-list allowed|denied|absent|error <errno>    opendir(SSH_DIR)
 *   ssh-open allowed|denied|absent|error <errno>    open(SSH_FILE), never read
 *   write-outside allowed|denied|skipped|...        create OUTSIDE (then remove)
 *   write-tmp allowed|denied|...                    create TMP_FILE (then remove)
 *   read-tmp allowed|denied|...                     open READ_FILE, never read
 *   write-container allowed|denied|...              create a file under $HOME
 *
 * usage: sandbox-probe SSH_DIR SSH_FILE OUTSIDE|- TMP_FILE READ_FILE
 * It reads no file contents and prints no directory entry name.
 */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

int sandbox_check(pid_t pid, const char *operation, int type, ...);

static void report(const char *label, int ok, int error) {
    if (ok) {
        printf("%s allowed\n", label);
    } else if (error == EPERM || error == EACCES) {
        printf("%s denied\n", label);
    } else if (error == ENOENT) {
        printf("%s absent\n", label);
    } else {
        printf("%s error %d\n", label, error);
    }
}

static void try_open(const char *label, const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    int error = errno;
    if (fd >= 0) {
        close(fd);
    }
    report(label, fd >= 0, error);
}

static void try_create(const char *label, const char *path) {
    int fd = open(path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0600);
    int error = errno;
    if (fd >= 0) {
        close(fd);
        unlink(path);
    }
    report(label, fd >= 0, error);
}

int main(int argc, char **argv) {
    if (argc != 6) {
        fprintf(stderr, "usage: sandbox-probe SSH_DIR SSH_FILE OUTSIDE|- TMP_FILE READ_FILE\n");
        return 2;
    }
    printf("sandboxed %s\n", sandbox_check(getpid(), NULL, 0) == 1 ? "yes" : "no");

    DIR *directory = opendir(argv[1]);
    int error = errno;
    if (directory != NULL) {
        closedir(directory);
    }
    report("ssh-list", directory != NULL, error);
    try_open("ssh-open", argv[2]);

    if (strcmp(argv[3], "-") == 0) {
        printf("write-outside skipped\n");
    } else {
        try_create("write-outside", argv[3]);
    }
    try_create("write-tmp", argv[4]);
    try_open("read-tmp", argv[5]);

    /* In the App Sandbox, HOME is the container's Data directory. */
    const char *home = getenv("HOME");
    char inside[PATH_MAX];
    if (home == NULL || snprintf(inside, sizeof inside, "%s/sandbox-probe-%d", home, (int)getpid()) >= (int)sizeof inside) {
        printf("write-container error %d\n", ENAMETOOLONG);
    } else {
        try_create("write-container", inside);
    }
    return fflush(stdout) == 0 ? 0 : 1;
}
