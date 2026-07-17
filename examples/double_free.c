#include <stdlib.h>

void twice(void) {
    char *p = malloc(8);
    free(p);
    free(p);
}
