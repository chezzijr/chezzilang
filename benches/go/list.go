package main
import "fmt"
func main() {
	xs := []int{}
	for i := 0; i < 2000000; i++ { xs = append(xs, i) }
	total := 0
	for _, x := range xs { total += x }
	fmt.Println(total)
}
