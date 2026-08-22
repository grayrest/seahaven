## Represents an I/O error that can occur during platform operations.
##
## **NotFound** - An entity was not found, often a file.
##
## **PermissionDenied** - The operation lacked the necessary privileges to complete.
##
## **BrokenPipe** - The operation failed because a pipe was closed.
##
## **AlreadyExists** - An entity already exists, often a file.
##
## **Interrupted** - This operation was interrupted. Interrupted operations can typically be retried.
##
## **IsADirectory** - A filesystem operation expected a non-directory path.
##
## **NotADirectory** - A filesystem operation expected a directory path.
##
## **Unsupported** - This operation is unsupported on this platform. This means that the operation can never succeed.
##
## **OutOfMemory** - An operation could not be completed, because it failed to allocate enough memory.
##
## **Other** - A custom error that does not fall under any other I/O error kind.
IOErr := [
	AlreadyExists,
	BrokenPipe,
	Interrupted,
	IsADirectory,
	NotFound,
	NotADirectory,
	Other(Str),
	OutOfMemory,
	PermissionDenied,
	Unsupported,
].{

	## Convert an I/O error to a concise human-readable message.
	to_str : IOErr -> Str
	to_str = |err|
		match err {
			AlreadyExists => "entity already exists"
			BrokenPipe => "pipe is closed"
			Interrupted => "operation was interrupted"
			IsADirectory => "expected a non-directory path, but found a directory"
			NotFound => "entity was not found"
			NotADirectory => "expected a directory, but found a non-directory path"
			Other(message) => message
			OutOfMemory => "operation could not allocate enough memory"
			PermissionDenied => "permission denied"
			Unsupported => "operation is unsupported"
		}
}

expect IOErr.to_str(NotFound) == "entity was not found"
expect IOErr.to_str(Other("device unavailable")) == "device unavailable"
