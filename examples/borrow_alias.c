/* borrow_alias.c — 借用（&x）の別名圧力を見る例（W8: ADR-0010）
   ヒープを一切使わなくても「同じ場所に同時に書ける名前」は増える。
   ここは leak も use-after-free も無い健全なコードだが、
   書込可能な別名の数（＝リファクタリング時に同時に追う名前の数）は測れる */
#include <stdio.h>

/* [1] 書込可能な借用別名が同時に2つ: 別名圧力 = 2（warn）
   p と q はどちらも x に書ける。x の最終値を知るには
   2つの名前を同時に追う必要がある */
void borrow_alias(void) {
    int x = 0;
    int *p = &x;
    int *q = p;
    *p = 1;
    *q = 2;
    printf("%d\n", x);
}

/* [2] const 借用は圧力に数えない: 別名圧力 = 1
   r は読むだけの別名（const int *）。書き手は w の1名のままなので、
   別名が増えても追跡の負担は増えていない */
void const_borrow(void) {
    int x = 0;
    int *w = &x;
    const int *r = &x;
    *w = 5;
    printf("%d %d\n", *w, *r);
}
