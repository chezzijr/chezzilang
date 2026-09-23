// Go twin of benches/sched/body_and_spawn.chz (TICKET-168, W15-2): the main goroutine burns CPU
// while one spawned goroutine burns the same amount. GOMAXPROCS=1 runs them one at a time.
package main

import "fmt"

func burn(n int) int {
	s := 0
	for k := 0; k < n/5000000; k++ {
		for i := 0; i < 5000000; i++ {
			s = s + i%7
		}
	}
	return s
}

func main() {
	ch := make(chan int, 1)
	go func() { ch <- burn(300000000) }()
	x := burn(300000000)
	fmt.Println(x)
	fmt.Println(<-ch)
}
