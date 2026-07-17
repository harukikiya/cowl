#include <stdlib.h>
#include <string.h>

/* [1] 教科書コース: 確保 -> 使用 -> 解放 -> NULLリセット */
void textbook(void) {
    char *buf = malloc(64);
    if (!buf) return;
    strcpy(buf, "hello");
    free(buf);
    buf = NULL;
}

/* [2] リーク疑い: 関数終端まで free も脱出もしない */
void leaky(void) {
    int *nums = malloc(10 * sizeof(int));
    nums[0] = 42;
}

/* [3] double free */
void twice(void) {
    char *p = malloc(8);
    free(p);
    free(p);
}

/* [4] 別名経由の use-after-free */
void alias_uaf(void) {
    char *p = malloc(8);
    char *q = p;
    free(q);
    char c = *p;
    (void)c;
}

/* [5] 脱出はリークではない / 未知関数は「曖昧」 */
char *make_buf(void) {
    char *b = malloc(128);
    return b;
}

extern void register_buffer(char *b);
void ambiguous(void) {
    char *b = malloc(128);
    register_buffer(b);
    free(b);
}
