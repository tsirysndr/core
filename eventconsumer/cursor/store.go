package cursor

type Store interface {
	Set(key string, cursor int64)
	Get(key string) (cursor int64)
}
