#include <stdio.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    FILE *ready = fopen(argv[1], "w");
    if (!ready) return 3;
    fputs("ready\n", ready);
    fclose(ready);
    for (;;) pause();
}
