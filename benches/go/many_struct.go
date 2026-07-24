package main
import "fmt"
type P struct{ x, y int }
func main() {
	xs := []P{}
	for i := 0; i < 2000000; i++ { xs = append(xs, P{i, i}) }
	fmt.Println(len(xs))
}
