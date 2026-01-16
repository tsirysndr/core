package main

import (
	"context"
	"log"

	"tangled.org/core/appview/db"
)

func main() {
	_, err := db.Make(context.Background(), "./tmp/appview.db")
	if err != nil {
		log.Fatalln("failed to make db:", err)
	}
}
