## Default Permission

Nostos Tauri plugin — default permissions for the connect/subscribe/write/query/checkpoint/watch surface. A shipped plugin would add scoped per-table permission sets; this scaffold grants all six commands unconditionally.

#### This default permission set includes the following:

- `allow-connect`
- `allow-subscribe`
- `allow-write`
- `allow-query`
- `allow-checkpoint`
- `allow-watch`

## Permission Table

<table>
<tr>
<th>Identifier</th>
<th>Description</th>
</tr>


<tr>
<td>

`nostos-tauri:allow-checkpoint`

</td>
<td>

Enables the checkpoint command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos-tauri:deny-checkpoint`

</td>
<td>

Denies the checkpoint command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos-tauri:allow-connect`

</td>
<td>

Enables the connect command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos-tauri:deny-connect`

</td>
<td>

Denies the connect command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos-tauri:allow-query`

</td>
<td>

Enables the query command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos-tauri:deny-query`

</td>
<td>

Denies the query command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos-tauri:allow-subscribe`

</td>
<td>

Enables the subscribe command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos-tauri:deny-subscribe`

</td>
<td>

Denies the subscribe command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos-tauri:allow-watch`

</td>
<td>

Enables the watch command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos-tauri:deny-watch`

</td>
<td>

Denies the watch command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos-tauri:allow-write`

</td>
<td>

Enables the write command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos-tauri:deny-write`

</td>
<td>

Denies the write command without any pre-configured scope.

</td>
</tr>
</table>
