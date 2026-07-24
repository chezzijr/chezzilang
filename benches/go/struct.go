package main
import "fmt"
type Acc struct{ a, b, c, d, e, f, g, h int }
func main() {
	s := Acc{1, 2, 3, 4, 5, 6, 7, 8}
	total := 0
	for i := 0; i < 1000000; i++ {
		total = total + s.a + s.b + s.c + s.d + s.e + s.f + s.g + s.h
		s.a = s.b; s.c = s.d; s.e = s.f; s.g = s.h
	}
	fmt.Println(total)
}
