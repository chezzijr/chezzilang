package main
import "fmt"
type P struct{ x, y int }
func main() {
	m := map[int]P{}
	for i := 0; i < 1000000; i++ { m[i] = P{i, i} }
	fmt.Println(len(m))
}
