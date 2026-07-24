package main
import "fmt"
func main() {
	m := map[int]int{}
	for i := 0; i < 200000; i++ { m[i] = i * 2 }
	total := 0
	for r := 0; r < 5; r++ {
		for j := 0; j < 200000; j++ { total += m[j] }
	}
	fmt.Println(total)
}
