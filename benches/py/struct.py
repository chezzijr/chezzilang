# Bench: struct field read/write throughput, ~1M iterations. Behavior-identical to
# benches/chz/struct.chz.

class Acc:
    def __init__(self, a, b, c, d, e, f, g, h):
        self.a = a
        self.b = b
        self.c = c
        self.d = d
        self.e = e
        self.f = f
        self.g = g
        self.h = h


def main():
    s = Acc(1, 2, 3, 4, 5, 6, 7, 8)
    total = 0
    i = 0
    while i < 1000000:
        total = total + s.a + s.b + s.c + s.d + s.e + s.f + s.g + s.h
        s.a = s.b
        s.c = s.d
        s.e = s.f
        s.g = s.h
        i += 1
    print(total)


main()
