# Bench: polymorphic (megamorphic) method dispatch throughput. Behavior-identical to
# benches/chz/poly_method.chz — four shape classes, one heterogeneous list walked at one call site.
# ~4M method calls. Prints 34000000.


class Sq:
    def __init__(self, s):
        self.s = s

    def area(self):
        return self.s * self.s


class Rect:
    def __init__(self, w, h):
        self.w = w
        self.h = h

    def area(self):
        return self.w * self.h


class Tri:
    def __init__(self, b, hh):
        self.b = b
        self.hh = hh

    def area(self):
        return self.b * self.hh // 2


class Circ:
    def __init__(self, r):
        self.r = r

    def area(self):
        return self.r + self.r + 1


def total_area(shapes):
    acc = 0
    for s in shapes:
        acc = acc + s.area()
    return acc


def main():
    shapes = []
    shapes.append(Sq(3))       # 9
    shapes.append(Rect(2, 4))  # 8
    shapes.append(Tri(4, 5))   # 10
    shapes.append(Circ(3))     # 7
    total = 0
    i = 0
    while i < 1000000:
        total = total + total_area(shapes)
        i += 1
    print(total)


main()
