// Linux-only syscall probe for the real file-server process tests.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static void record(const char *event) {
    const char *trace = getenv("STARTUP_SYNC_TRACE");
    if (!trace) return;
    int fd = open(trace, O_WRONLY | O_CREAT | O_APPEND, 0600);
    if (fd < 0) _exit(90);
    size_t length = strlen(event);
    if (write(fd, event, length) != (ssize_t)length || write(fd, "\n", 1) != 1)
        _exit(91);
    close(fd);
}

int fsync(int fd) {
    int (*real_fsync)(int) = dlsym(RTLD_NEXT, "fsync");
    char link[64], path[PATH_MAX];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
    ssize_t length = readlink(link, path, sizeof(path) - 1);
    if (length < 0) _exit(92);
    path[length] = '\0';
    record(path);
    const char *failure = getenv("STARTUP_SYNC_FAIL");
    if (failure && strcmp(path, failure) == 0) {
        errno = EIO;
        return -1;
    }
    return real_fsync(fd);
}

int bind(int fd, const struct sockaddr *address, socklen_t length) {
    int (*real_bind)(int, const struct sockaddr *, socklen_t) = dlsym(RTLD_NEXT, "bind");
    record("BIND");
    return real_bind(fd, address, length);
}
