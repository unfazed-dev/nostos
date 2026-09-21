## Default Permission

Nostos Tauri plugin — default permissions for the connect/subscribe/write/query/checkpoint/watch/set-token/sign-out + push-token register/deregister surface. A shipped plugin would add scoped per-table permission sets; this scaffold grants all ten commands unconditionally.

#### This default permission set includes the following:

- `allow-connect`
- `allow-subscribe`
- `allow-write`
- `allow-query`
- `allow-checkpoint`
- `allow-watch`
- `allow-set-token`
- `allow-sign-out`
- `allow-register-push-token`
- `allow-deregister-push-token`
- `allow-or-set-add`
- `allow-or-set-remove`
- `allow-counter-increment`
- `allow-counter-decrement`
- `allow-dead-letters`
- `allow-connection-state`

## Permission Table

<table>
<tr>
<th>Identifier</th>
<th>Description</th>
</tr>


<tr>
<td>

`cairn:allow-checkpoint`

</td>
<td>

Enables the checkpoint command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-checkpoint`

</td>
<td>

Denies the checkpoint command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-connect`

</td>
<td>

Enables the connect command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-connect`

</td>
<td>

Denies the connect command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-connection-state`

</td>
<td>

Enables the connection_state command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-connection-state`

</td>
<td>

Denies the connection_state command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-counter-decrement`

</td>
<td>

Enables the counter_decrement command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-counter-decrement`

</td>
<td>

Denies the counter_decrement command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-counter-increment`

</td>
<td>

Enables the counter_increment command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-counter-increment`

</td>
<td>

Denies the counter_increment command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-dead-letters`

</td>
<td>

Enables the dead_letters command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-dead-letters`

</td>
<td>

Denies the dead_letters command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-deregister-push-token`

</td>
<td>

Enables the deregister_push_token command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-deregister-push-token`

</td>
<td>

Denies the deregister_push_token command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-or-set-add`

</td>
<td>

Enables the or_set_add command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-or-set-add`

</td>
<td>

Denies the or_set_add command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-or-set-remove`

</td>
<td>

Enables the or_set_remove command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-or-set-remove`

</td>
<td>

Denies the or_set_remove command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-query`

</td>
<td>

Enables the query command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-query`

</td>
<td>

Denies the query command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-register-push-token`

</td>
<td>

Enables the register_push_token command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-register-push-token`

</td>
<td>

Denies the register_push_token command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-set-token`

</td>
<td>

Enables the set_token command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-set-token`

</td>
<td>

Denies the set_token command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-sign-out`

</td>
<td>

Enables the sign_out command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-sign-out`

</td>
<td>

Denies the sign_out command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-subscribe`

</td>
<td>

Enables the subscribe command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-subscribe`

</td>
<td>

Denies the subscribe command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-watch`

</td>
<td>

Enables the watch command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-watch`

</td>
<td>

Denies the watch command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:allow-write`

</td>
<td>

Enables the write command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`cairn:deny-write`

</td>
<td>

Denies the write command without any pre-configured scope.

</td>
</tr>
</table>
