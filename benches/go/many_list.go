package main
import "fmt"
func main() {
	xs := [][]int{}
	for i := 0; i < 2000000; i++ { xs = append(xs, []int{i, i}) }
	fmt.Println(len(xs))
}
