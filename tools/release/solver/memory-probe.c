/*
 * SV-P6 fixture (security-policy.md, PL4a implementation contract, "SV-P6
 * method"): does macOS enforce a resource limit on a child's memory?
 *
 *   memory-probe none|as|data|rss MIB CHUNKS
 *
 * Forks a child. The child sets the named limit to MIB MiB (soft and hard),
 * then allocates and touches CHUNKS blocks of 64 MiB (1 to 32). It prints
 * "chunk N ok" or "chunk N refused" per block, then "done", and waits for
 * the end of its standard input. The parent reads the child's lines, reads
 * its phys_footprint with proc_pid_rusage, closes the pipe, reaps it, and
 * prints the exit status and ru_maxrss. There is no timer: the probe ends
 * by the child's last line and the closed pipe.
 */
#include <errno.h>
#include <libproc.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHUNK ((size_t)64 << 20)

static int child(int resource, rlim_t bytes, int chunks, int in_fd, int out_fd)
{
    FILE *out = fdopen(out_fd, "w");
    if (out == NULL)
        return 3;
    if (resource >= 0) {
        struct rlimit limit = { bytes, bytes };
        if (setrlimit(resource, &limit) != 0) {
            fprintf(out, "setrlimit failed: %s\n", strerror(errno));
            fclose(out);
            return 4;
        }
        struct rlimit now;
        getrlimit(resource, &now);
        fprintf(out, "limit set: cur %llu max %llu\n", (unsigned long long)now.rlim_cur,
                (unsigned long long)now.rlim_max);
    }
    for (int i = 1; i <= chunks; i++) {
        /* volatile: the stores must reach memory, or the compiler drops them. */
        volatile char *block = malloc(CHUNK);
        if (block == NULL) {
            fprintf(out, "chunk %d refused\n", i);
            fflush(out);
            break;
        }
        for (size_t o = 0; o < CHUNK; o += 4096)
            block[o] = (char)i;
        fprintf(out, "chunk %d ok\n", i);
        fflush(out);
    }
    fprintf(out, "done\n");
    fflush(out);
    char byte;
    while (read(in_fd, &byte, 1) > 0) {
    }
    return 0;
}

int main(int argc, char **argv)
{
    if (argc != 4) {
        fprintf(stderr, "usage: memory-probe none|as|data|rss MIB CHUNKS\n");
        return 2;
    }
    int resource;
    if (strcmp(argv[1], "none") == 0)
        resource = -1;
    else if (strcmp(argv[1], "as") == 0)
        resource = RLIMIT_AS;
    else if (strcmp(argv[1], "data") == 0)
        resource = RLIMIT_DATA;
    else if (strcmp(argv[1], "rss") == 0)
        resource = RLIMIT_RSS;
    else {
        fprintf(stderr, "memory-probe: unknown limit %s\n", argv[1]);
        return 2;
    }
    long mib = strtol(argv[2], NULL, 10);
    long chunks = strtol(argv[3], NULL, 10);
    if (mib < 1 || mib > 65536 || chunks < 1 || chunks > 32) {
        fprintf(stderr, "memory-probe: MIB 1 to 65536, CHUNKS 1 to 32\n");
        return 2;
    }
    int to_child[2], from_child[2];
    if (pipe(to_child) != 0 || pipe(from_child) != 0) {
        perror("pipe");
        return 1;
    }
    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return 1;
    }
    if (pid == 0) {
        close(to_child[1]);
        close(from_child[0]);
        _exit(child(resource, (rlim_t)mib << 20, (int)chunks, to_child[0], from_child[1]));
    }
    close(to_child[0]);
    close(from_child[1]);
    FILE *in = fdopen(from_child[0], "r");
    char line[256];
    int done = 0;
    while (in != NULL && fgets(line, sizeof line, in) != NULL) {
        printf("child: %s", line);
        if (strcmp(line, "done\n") == 0) {
            done = 1;
            break;
        }
    }
    if (done) {
        struct rusage_info_v4 info;
        if (proc_pid_rusage(pid, RUSAGE_INFO_V4, (rusage_info_t *)&info) == 0)
            printf("phys_footprint %llu MiB\n", (unsigned long long)(info.ri_phys_footprint >> 20));
        else
            printf("proc_pid_rusage failed: %s\n", strerror(errno));
    }
    close(to_child[1]);
    int status = 0;
    struct rusage usage;
    if (wait4(pid, &status, 0, &usage) < 0) {
        perror("wait4");
        return 1;
    }
    if (WIFEXITED(status))
        printf("exit %d", WEXITSTATUS(status));
    else if (WIFSIGNALED(status))
        printf("signal %d", WTERMSIG(status));
    printf(" maxrss %ld MiB\n", usage.ru_maxrss >> 20);
    return 0;
}
