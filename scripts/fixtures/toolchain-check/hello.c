#include <stdio.h>
#include <string.h>

int main(void) {
    char buf[64];
    snprintf(buf, sizeof buf, "hello from C, strerror(0)=%s", strerror(0));
    puts(buf);
    return 0;
}
