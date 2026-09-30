export const closeMenu = () => {
  const menuState = document.querySelector<HTMLInputElement>('#menustate');
  if (menuState != null) {
    menuState.checked = false;
  }
};
